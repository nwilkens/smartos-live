// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.
//
// Copyright 2026 Edgecast Cloud LLC.

//! The production [`ZoneHost`] + [`NicTagRegistrar`]: real
//! `zonecfg`/`zoneadm`/`zfs`/`svccfg`/`nictagadm`. Reuses
//! `tritond_cn_platform`'s `ZoneadmTool` for the zone-existence lookup;
//! everything else shells out directly so the argv matches node `VM.js`
//! exactly (the differential harness checks this). Not unit-tested (it
//! drives the live host); the create flow is tested via `FakeHost`.

use std::process::Stdio;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use tokio::io::AsyncWriteExt as _;
use tokio::process::Command;
use tritond_cn_platform::smartos::zoneadm::{ZoneadmError, ZoneadmTool};
use uuid::Uuid;

use crate::backend::traits::{
    HostFacts, InstallArgs, NicTagRegistrar, ProvisionOutcome, SnapshotInfo, ZoneHost, ZoneKstat,
    ZoneRef, ZoneRuntime,
};
use crate::error::{Result, VmadmError};

const ZONECFG: &str = "/usr/sbin/zonecfg";
const ZONEADM: &str = "/usr/sbin/zoneadm";
const ZFS: &str = "/usr/sbin/zfs";
const SVCCFG: &str = "/usr/sbin/svccfg";
const NICTAGADM: &str = "/usr/bin/nictagadm";
const KSTAT: &str = "/usr/bin/kstat";

/// Production host backend.
#[derive(Debug, Clone, Default)]
pub struct HostBackend;

impl HostBackend {
    #[must_use]
    pub fn new() -> Self {
        Self
    }
}

/// A non-zero exit is tolerated (treated as success) when the trimmed
/// stderr matches one of a command's expected "already gone / already in
/// this state" phrases. node anchors these with `trim(stderr).match(/...$/)`
/// (e.g. `VM.js` line 17979 `/zone is already halted$/`), so we match the
/// phrase at the END of the trimmed stderr OR at the end of any single
/// trimmed line — never an unanchored substring that could also appear
/// inside an unrelated failure message. Each command passes only its own
/// phrases (command-scoped), so a broad fragment can't leak across calls.
fn stderr_tolerated(stderr: &str, tolerate: &[&str]) -> bool {
    if tolerate.is_empty() {
        return false;
    }
    let trimmed = stderr.trim();
    tolerate.iter().any(|t| {
        trimmed.ends_with(t) || stderr.lines().any(|line| line.trim_end().ends_with(t))
    })
}

/// Run a command, optionally feeding `stdin`, with extra environment.
/// `tolerate` phrases that end a trimmed stderr line turn a non-zero exit
/// into success (idempotent teardown: "already gone" is fine). See
/// [`stderr_tolerated`] for the anchoring rules.
async fn run(
    bin: &str,
    args: &[&str],
    stdin: Option<&str>,
    env: &[(&str, &str)],
    tolerate: &[&str],
) -> Result<String> {
    let mut cmd = Command::new(bin);
    cmd.args(args);
    for (k, v) in env {
        cmd.env(k, v);
    }
    cmd.stdin(if stdin.is_some() {
        Stdio::piped()
    } else {
        Stdio::null()
    });
    cmd.stdout(Stdio::piped()).stderr(Stdio::piped());

    let mut child = cmd.spawn().map_err(|e| VmadmError::Spawn {
        bin: bin.to_string(),
        source: e,
    })?;
    if let Some(data) = stdin {
        let mut sink = child
            .stdin
            .take()
            .ok_or_else(|| VmadmError::Payload(format!("{bin}: no stdin pipe")))?;
        sink.write_all(data.as_bytes())
            .await
            .map_err(|e| VmadmError::Spawn {
                bin: bin.to_string(),
                source: e,
            })?;
        sink.shutdown().await.map_err(|e| VmadmError::Spawn {
            bin: bin.to_string(),
            source: e,
        })?;
    }
    let out = child
        .wait_with_output()
        .await
        .map_err(|e| VmadmError::Spawn {
            bin: bin.to_string(),
            source: e,
        })?;
    if out.status.success() {
        return Ok(String::from_utf8_lossy(&out.stdout).into_owned());
    }
    let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
    if stderr_tolerated(&stderr, tolerate) {
        return Ok(String::new());
    }
    Err(VmadmError::NonZeroExit {
        bin: bin.to_string(),
        status: out.status,
        stderr,
    })
}

fn zone_flag(zone: &ZoneRef) -> (&'static str, String) {
    match zone {
        ZoneRef::Zonename(n) => ("-z", n.clone()),
        ZoneRef::Uuid(u) => ("-u", u.to_string()),
    }
}

#[async_trait]
impl ZoneHost for HostBackend {
    async fn zone_configured(&self, uuid: Uuid) -> Result<bool> {
        match ZoneadmTool::new().lookup(&uuid.to_string()).await {
            Ok(_) => Ok(true),
            Err(ZoneadmError::NotFound { .. }) => Ok(false),
            Err(e) => Err(e.into()),
        }
    }

    async fn zonecfg(&self, zone: ZoneRef, doc: &str) -> Result<()> {
        let (flag, id) = zone_flag(&zone);
        run(ZONECFG, &[flag, &id, "-f", "-"], Some(doc), &[], &[]).await?;
        Ok(())
    }

    async fn zoneadm_install(&self, args: &InstallArgs) -> Result<()> {
        let quota = args.quota_gib.to_string();
        let mut argv: Vec<String> = vec![
            "-z".into(),
            args.zonename.clone(),
            "install".into(),
            "-q".into(),
            quota,
        ];
        if let Some(image) = args.image_uuid {
            argv.push("-t".into());
            argv.push(image.to_string());
        }
        if args.no_dataset {
            argv.push("-x".into());
            argv.push("nodataset".into());
        }
        let refs: Vec<&str> = argv.iter().map(String::as_str).collect();
        run(ZONEADM, &refs, None, &[], &[]).await?;
        Ok(())
    }

    async fn zoneadm_boot(&self, uuid: Uuid) -> Result<()> {
        run(ZONEADM, &["-u", &uuid.to_string(), "boot"], None, &[], &[]).await?;
        Ok(())
    }

    async fn zfs_create(&self, dataset: &str, props: &[(String, String)]) -> Result<()> {
        let mut argv: Vec<String> = vec!["create".into()];
        for (k, v) in props {
            argv.push("-o".into());
            argv.push(format!("{k}={v}"));
        }
        argv.push(dataset.to_string());
        let refs: Vec<&str> = argv.iter().map(String::as_str).collect();
        run(ZFS, &refs, None, &[], &[]).await?;
        Ok(())
    }

    async fn zfs_snapshot_exists(&self, snapshot: &str) -> Result<bool> {
        let status = Command::new(ZFS)
            .args(["get", "-Ho", "value", "name", snapshot])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .await
            .map_err(|e| VmadmError::Spawn {
                bin: ZFS.to_string(),
                source: e,
            })?;
        Ok(status.success())
    }

    async fn zfs_dataset_exists(&self, dataset: &str) -> Result<bool> {
        // Same existence probe as `zfs_snapshot_exists`; a dataset and a
        // snapshot are both resolvable by `zfs get name`.
        let status = Command::new(ZFS)
            .args(["get", "-Ho", "value", "name", dataset])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .await
            .map_err(|e| VmadmError::Spawn {
                bin: ZFS.to_string(),
                source: e,
            })?;
        Ok(status.success())
    }

    async fn zfs_snapshot(&self, snapshot: &str) -> Result<()> {
        run(ZFS, &["snapshot", snapshot], None, &[], &[]).await?;
        Ok(())
    }

    async fn zfs_clone(
        &self,
        snapshot: &str,
        dataset: &str,
        props: &[(String, String)],
    ) -> Result<()> {
        let mut argv: Vec<String> = vec!["clone".into(), "-F".into()];
        for (k, v) in props {
            argv.push("-o".into());
            argv.push(format!("{k}={v}"));
        }
        argv.push(snapshot.to_string());
        argv.push(dataset.to_string());
        let refs: Vec<&str> = argv.iter().map(String::as_str).collect();
        run(ZFS, &refs, None, &[], &[]).await?;
        Ok(())
    }

    async fn zfs_create_volume(
        &self,
        dataset: &str,
        size_mib: u64,
        props: &[(String, String)],
    ) -> Result<()> {
        let mut argv: Vec<String> = vec!["create".into()];
        for (k, v) in props {
            argv.push("-o".into());
            argv.push(format!("{k}={v}"));
        }
        argv.push("-V".into());
        argv.push(format!("{size_mib}M"));
        argv.push(dataset.to_string());
        let refs: Vec<&str> = argv.iter().map(String::as_str).collect();
        run(ZFS, &refs, None, &[], &[]).await?;
        Ok(())
    }

    async fn zfs_set(&self, dataset: &str, props: &[(String, String)]) -> Result<()> {
        let mut argv: Vec<String> = vec!["set".into()];
        for (k, v) in props {
            argv.push(format!("{k}={v}"));
        }
        argv.push(dataset.to_string());
        let refs: Vec<&str> = argv.iter().map(String::as_str).collect();
        run(ZFS, &refs, None, &[], &[]).await?;
        Ok(())
    }

    async fn zfs_list_volumes(&self, dataset: &str) -> Result<Vec<(String, u64)>> {
        let out = run(
            ZFS,
            &["list", "-Hrpt", "volume", "-o", "name,volsize", dataset],
            None,
            &[],
            &[],
        )
        .await?;
        Ok(out
            .lines()
            .filter_map(|line| {
                let mut f = line.split('\t');
                let name = f.next()?.to_string();
                let size = f.next()?.parse::<u64>().ok()?;
                Some((name, size))
            })
            .collect())
    }

    async fn zfs_sum_volume_refreservation(&self, dataset: &str) -> Result<u64> {
        let out = run(
            ZFS,
            &[
                "list",
                "-Hpr",
                "-t",
                "volume",
                "-o",
                "refreservation",
                dataset,
            ],
            None,
            &[],
            &[],
        )
        .await?;
        Ok(out
            .lines()
            .filter_map(|l| l.trim().parse::<u64>().ok())
            .sum())
    }

    async fn build_nocloud_seed(
        &self,
        raw_zvol_path: &str,
        files: &crate::seed::SeedFiles,
    ) -> Result<()> {
        build_nocloud_seed_pcfs(raw_zvol_path, files).await
    }

    async fn zone_info(&self, uuid: Uuid) -> Result<Option<ZoneRuntime>> {
        // `ZoneadmTool::lookup` parses the first six fields but drops the
        // optional `zonedid` (field 8). node reads it for the `zonedid`
        // property, so re-run `zoneadm list -p` here and parse the full line.
        let out = run(
            ZONEADM,
            &["-z", &uuid.to_string(), "-u", &uuid.to_string(), "list", "-p"],
            None,
            &[],
            &["No such zone configured", "could not find zone"],
        )
        .await;
        let line = match out {
            Ok(s) => s.lines().find(|l| !l.trim().is_empty()).map(str::to_string),
            Err(VmadmError::NonZeroExit { stderr, .. })
                if stderr.contains("No such zone configured")
                    || stderr.contains("could not find zone") =>
            {
                None
            }
            Err(e) => return Err(e),
        };
        let Some(line) = line else { return Ok(None) };
        let fields: Vec<&str> = line.trim_end().split(':').collect();
        if fields.len() < 6 {
            return Ok(None);
        }
        let parse_id = |s: &str| match s {
            "-" | "" => None,
            other => other.parse::<i64>().ok().filter(|n| *n != -1),
        };
        Ok(Some(ZoneRuntime {
            zoneid: parse_id(fields[0]),
            state: fields[2].to_string(),
            zonepath: fields[3].to_string(),
            brand: fields[5].to_string(),
            zonedid: fields.get(7).and_then(|f| parse_id(f)),
        }))
    }

    async fn list_zone_uuids(&self) -> Result<Vec<Uuid>> {
        let out = run(ZONEADM, &["list", "-cp"], None, &[], &[]).await?;
        // Format: zoneid:zonename:state:zonepath:uuid:brand:ip-type[...]
        Ok(out
            .lines()
            .filter_map(|line| {
                let f: Vec<&str> = line.split(':').collect();
                f.get(4).and_then(|u| u.parse::<Uuid>().ok())
            })
            .collect())
    }

    async fn zoneadm_halt(&self, uuid: Uuid) -> Result<()> {
        // `-X` forces the power-off, bypassing brand statechange hooks so a
        // wedged hook can't hang stop/delete (node `VM.js` lines 12956/17976
        // `zoneadm(['-u', uuid, 'halt', '-X'])`). Tolerate only node's
        // already-halted phrase (`VM.js` line 17979 `/zone is already
        // halted$/`); the old over-broad "is not in"/"not running" fragments
        // could swallow unrelated halt failures.
        run(
            ZONEADM,
            &["-u", &uuid.to_string(), "halt", "-X"],
            None,
            &[],
            &["zone is already halted"],
        )
        .await?;
        Ok(())
    }

    async fn zoneadm_reboot(&self, uuid: Uuid) -> Result<()> {
        run(
            ZONEADM,
            &["-u", &uuid.to_string(), "reboot"],
            None,
            &[],
            &[],
        )
        .await?;
        Ok(())
    }

    async fn zlogin_shutdown(&self, uuid: Uuid, timeout: Duration) -> Result<()> {
        let id = uuid.to_string();
        let args = [id.as_str(), "shutdown", "-y", "-i", "5", "-g", "0"];
        let fut = run("/usr/sbin/zlogin", &args, None, &[], &[]);
        match tokio::time::timeout(timeout, fut).await {
            Ok(r) => r.map(|_| ()),
            // The graceful path timed out; the caller falls back to a halt.
            Err(_) => Err(VmadmError::Payload(format!(
                "zlogin shutdown of {uuid} timed out"
            ))),
        }
    }

    async fn zonecfg_export(&self, uuid: Uuid) -> Result<String> {
        run(
            ZONECFG,
            &["-u", &uuid.to_string(), "export"],
            None,
            &[],
            &[],
        )
        .await
    }

    async fn zfs_get_props(
        &self,
        dataset: &str,
        props: &[&str],
    ) -> Result<std::collections::BTreeMap<String, String>> {
        let joined = props.join(",");
        let out = run(
            ZFS,
            &["get", "-Hp", "-o", "property,value", &joined, dataset],
            None,
            &[],
            &[],
        )
        .await?;
        Ok(out
            .lines()
            .filter_map(|line| {
                let mut f = line.split('\t');
                let p = f.next()?.to_string();
                let v = f.next()?.to_string();
                Some((p, v))
            })
            .collect())
    }

    async fn zone_kstat(&self, zoneid: i64, zonename: &str) -> Result<ZoneKstat> {
        // node reads these from the zones:::zone_misc kstat. We parse
        // `kstat -p zones:<id>:` and match the `zonename` data field, since
        // the kstat instance name is truncated to 30 chars.
        let stat = run(KSTAT, &["-p", &format!("zones:{zoneid}:")], None, &[], &[]).await?;
        let mut k = ZoneKstat::default();
        let mut matched = false;
        let mut fields: std::collections::HashMap<&str, &str> = std::collections::HashMap::new();
        for line in stat.lines() {
            let Some((key, val)) = line.rsplit_once('\t') else {
                continue;
            };
            let Some(field) = key.rsplit(':').next() else {
                continue;
            };
            fields.insert(field, val.trim());
        }
        if fields.get("zonename") == Some(&zonename) {
            matched = true;
        }
        if matched {
            k.init_pid = fields.get("init_pid").and_then(|v| v.parse().ok());
            k.init_restarts = fields.get("init_restarts").and_then(|v| v.parse().ok());
            k.boot_time = fields.get("boot_time").and_then(|v| v.parse().ok());
        }
        Ok(k)
    }

    async fn host_facts(&self) -> Result<HostFacts> {
        // node reads sysinfo's cache file /tmp/.sysinfo.json. Fall back to
        // invoking sysinfo if the cache is missing.
        let raw = match tokio::fs::read("/tmp/.sysinfo.json").await {
            Ok(bytes) => bytes,
            Err(_) => run("/usr/bin/sysinfo", &[], None, &[], &[])
                .await?
                .into_bytes(),
        };
        let v: serde_json::Value = serde_json::from_slice(&raw)
            .map_err(|e| VmadmError::Payload(format!("parse sysinfo: {e}")))?;
        Ok(HostFacts {
            server_uuid: v.get("UUID").and_then(|s| s.as_str()).and_then(|s| s.parse().ok()),
            platform_buildstamp: v
                .get("Live Image")
                .and_then(|s| s.as_str())
                .map(str::to_string),
        })
    }

    async fn zfs_list_snapshots(&self, dataset: &str) -> Result<Vec<SnapshotInfo>> {
        let out = run(
            ZFS,
            &[
                "list",
                "-Hpt",
                "snapshot",
                "-o",
                "name,used,creation",
                "-r",
                dataset,
            ],
            None,
            &[],
            &["does not exist", "dataset does not exist"],
        )
        .await?;
        Ok(out
            .lines()
            .filter_map(|line| {
                let mut f = line.split('\t');
                let name = f.next()?.to_string();
                let used = f.next()?.parse::<u64>().ok()?;
                let creation = f.next()?.parse::<i64>().ok()?;
                Some(SnapshotInfo {
                    name,
                    used,
                    creation,
                })
            })
            .collect())
    }

    async fn zfs_holds(&self, snapshot: &str) -> Result<Vec<(String, String)>> {
        // `-H`: no header, tab-delimited. node parses the human format and
        // splits out the tag (`VM.js` lines 8093-8135); `-H` gives us
        // `<name>\t<tag>\t<timestamp>` directly. A missing snapshot is an
        // empty hold list, not an error (mirrors node treating "does not
        // exist" / "could not find" as no holds).
        let out = run(
            ZFS,
            &["holds", "-H", snapshot],
            None,
            &[],
            &["does not exist", "could not find"],
        )
        .await?;
        Ok(out
            .lines()
            .filter_map(|line| {
                let mut f = line.split('\t');
                let name = f.next()?.to_string();
                let tag = f.next()?.to_string();
                Some((name, tag))
            })
            .collect())
    }

    async fn zfs_rename(&self, from: &str, to: &str) -> Result<()> {
        // `-f` force, matching node's reprovision renames (`VM.js` line 11761).
        run(ZFS, &["rename", "-f", from, to], None, &[], &[]).await?;
        Ok(())
    }

    async fn zfs_set_prop(&self, dataset: &str, prop: &str, value: &str) -> Result<()> {
        // Single `<prop>=<value>` (e.g. `zoned=off`), node `VM.js` line 11713.
        run(
            ZFS,
            &["set", &format!("{prop}={value}"), dataset],
            None,
            &[],
            &[],
        )
        .await?;
        Ok(())
    }

    async fn zonecfg_set_attr(&self, uuid: Uuid, name: &str, value: &str) -> Result<()> {
        // `select attr name=<name>; set value="<value>"; end` — node `VM.js`
        // lines 12159-12160 for the `dataset-uuid` (image_uuid) update.
        let doc = format!("select attr name={name}; set value=\"{value}\"; end\n");
        run(
            ZONECFG,
            &["-u", &uuid.to_string(), "-f", "-"],
            Some(&doc),
            &[],
            &[],
        )
        .await?;
        Ok(())
    }

    async fn zonecfg_remove_attr(&self, uuid: Uuid, name: &str) -> Result<()> {
        let doc = format!("remove attr name={name}\n");
        run(
            ZONECFG,
            &["-u", &uuid.to_string(), "-f", "-"],
            Some(&doc),
            &[],
            &[],
        )
        .await?;
        Ok(())
    }

    async fn zonecfg_remove_dataset(&self, uuid: Uuid, dataset: &str) -> Result<()> {
        // `remove dataset name=<ds>` — tolerate an already-absent resource so
        // a retried reprovision after a crash mid-dance does not fail here.
        let doc = format!("remove dataset name={dataset}\n");
        run(
            ZONECFG,
            &["-u", &uuid.to_string(), "-f", "-"],
            Some(&doc),
            &[],
            &["No such dataset resource", "resource not found"],
        )
        .await?;
        Ok(())
    }

    async fn zonecfg_add_dataset(&self, uuid: Uuid, dataset: &str) -> Result<()> {
        // `add dataset; set name=<ds>; end` — the same form create emits
        // (`VM.js` line 6001). Tolerate an already-present resource so a
        // re-run after the re-add succeeded is a no-op.
        let doc = format!("add dataset; set name={dataset}; end\n");
        run(
            ZONECFG,
            &["-u", &uuid.to_string(), "-f", "-"],
            Some(&doc),
            &[],
            &["already exists", "Invalid resource"],
        )
        .await?;
        Ok(())
    }

    async fn svccfg_mdata_exec_timeout(&self, zonepath: &str, secs: u64) -> Result<()> {
        let repo = format!("{zonepath}/root/etc/svc/repository.db");
        let secs = secs.to_string();
        run(
            SVCCFG,
            &[
                "-s",
                "svc:/smartdc/mdata:execute",
                "setprop",
                "start/timeout_seconds",
                "=",
                "count:",
                &secs,
            ],
            None,
            &[
                ("SVCCFG_CONFIGD_PATH", "/lib/svc/bin/svc.configd"),
                ("SVCCFG_REPOSITORY", &repo),
            ],
            &[],
        )
        .await?;
        Ok(())
    }

    async fn wait_provisioning(
        &self,
        zonepath: &str,
        timeout: Duration,
    ) -> Result<ProvisionOutcome> {
        // The in-zone mdata SMF renames <zoneroot>/var/svc/provisioning to
        // EITHER provision_success OR provision_failure. Wait for the
        // marker to move, then check which replaced it so a guest that
        // failed to provision is reported as a failure (not silently as a
        // healthy create).
        let root = format!("{zonepath}/root/var/svc");
        let marker = format!("{root}/provisioning");
        let failure = format!("{root}/provision_failure");
        let deadline = Instant::now() + timeout;
        loop {
            // A stat error is treated as "still provisioning" (not gone), so
            // a transient blip can't be read as premature success.
            if let Ok(false) = tokio::fs::try_exists(&marker).await {
                if tokio::fs::try_exists(&failure).await.unwrap_or(false) {
                    let reason = tokio::fs::read_to_string(&failure)
                        .await
                        .unwrap_or_default();
                    return Ok(ProvisionOutcome::Failure(reason.trim().to_string()));
                }
                return Ok(ProvisionOutcome::Success);
            }
            if Instant::now() >= deadline {
                return Ok(ProvisionOutcome::Timeout);
            }
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
    }

    async fn zfs_destroy_recursive(&self, dataset: &str) -> Result<()> {
        // `-rF` (uppercase): recurse and retry-on-EBUSY, matching node's
        // bhyve-disk teardown (`VM.js` lines 12754/12787 `destroy -rF`) so a
        // momentarily-busy dataset doesn't fail the delete. Still tolerant of
        // an already-gone dataset.
        run(
            ZFS,
            &["destroy", "-rF", dataset],
            None,
            &[],
            &["does not exist", "dataset does not exist"],
        )
        .await?;
        Ok(())
    }

    async fn zfs_list_sibling_disks(&self, disk_prefix: &str) -> Result<Vec<String>> {
        // List every volume and snapshot, then keep the ones named exactly
        // `<prefix>` or starting with it (`<prefix>0`, `<prefix>0@snap`).
        // The prefix already encodes the pool + zonename + `-disk`, so a
        // simple starts-with cannot stray onto an unrelated zone's datasets.
        let out = run(
            ZFS,
            &["list", "-H", "-o", "name", "-t", "volume,snapshot"],
            None,
            &[],
            // No pool / no datasets at all is not an error for us.
            &["no datasets available"],
        )
        .await?;
        Ok(out
            .lines()
            .map(str::trim)
            .filter(|name| {
                // The dataset (or the part before `@`) must equal the prefix
                // or be the prefix followed by an index digit, so `-disk`
                // matches `-disk0`/`-disk0@snap` but never `-diskette`.
                let base = name.split('@').next().unwrap_or(name);
                base == disk_prefix
                    || base
                        .strip_prefix(disk_prefix)
                        .is_some_and(|rest| rest.chars().next().is_some_and(|c| c.is_ascii_digit()))
            })
            .map(str::to_string)
            .collect())
    }

    async fn zoneadm_uninstall_force(&self, uuid: Uuid) -> Result<()> {
        run(
            ZONEADM,
            &["-u", &uuid.to_string(), "uninstall", "-F"],
            None,
            &[],
            // Only the genuinely-already-gone cases — NOT a bare "must be",
            // which would also swallow unrelated precondition failures
            // (e.g. "dataset must be ...") and leave residue.
            &["No such zone", "not installed", "in the configured state"],
        )
        .await?;
        Ok(())
    }

    async fn zonecfg_delete_force(&self, uuid: Uuid) -> Result<()> {
        run(
            ZONECFG,
            &["-u", &uuid.to_string(), "delete", "-F"],
            None,
            &[],
            &["No such zone", "could not be found"],
        )
        .await?;
        Ok(())
    }
}

/// Production nic_tag registrar. Verifies that ordinary tags
/// (admin/external/...) are already registered and fails closed otherwise.
/// Proteus dataplane pseudo-links (`proteusN`) are deliberately a no-op: see
/// [`NicTagRegistrar::ensure`] for why registering them is both useless and
/// harmful.
#[derive(Debug, Clone, Default)]
pub struct HostNicTagRegistrar;

impl HostNicTagRegistrar {
    #[must_use]
    pub fn new() -> Self {
        Self
    }
}

const MKFS: &str = "/usr/sbin/mkfs";
const MOUNT: &str = "/usr/sbin/mount";
const UMOUNT: &str = "/usr/sbin/umount";
const LOFIADM: &str = "/usr/sbin/lofiadm";
const MKTEMP: &str = "/usr/bin/mktemp";

/// Build the NoCloud CIDATA FAT16 image on `raw_zvol_path` using the same
/// native toolchain as node `cloudinit/lofs-fat16.js`: lofiadm-attach,
/// `mkfs -F pcfs -o b=CIDATA,nofdisk,fat=16,size=32768`, mount, write the
/// cidata files, then unmount + detach. Cleanup runs on any error.
async fn build_nocloud_seed_pcfs(
    raw_zvol_path: &str,
    files: &crate::seed::SeedFiles,
) -> Result<()> {
    let mount_point = run(MKTEMP, &["-d", "/tmp/lofs-fat16-XXXXXX"], None, &[], &[])
        .await?
        .trim()
        .to_string();

    let lofi = match run(LOFIADM, &["-a", raw_zvol_path], None, &[], &[]).await {
        Ok(out) => out.trim().to_string(),
        Err(e) => {
            let _ = tokio::fs::remove_dir(&mount_point).await;
            return Err(e);
        }
    };

    let result = seed_format_and_write(&lofi, &mount_point, files).await;

    // Always attempt cleanup, but do NOT hide a detach failure on the
    // success path (a leaked lofi device exhausts minors over many creates).
    let detach = run(LOFIADM, &["-d", &lofi], None, &[], &[]).await;
    let _ = tokio::fs::remove_dir(&mount_point).await;
    match (result, detach) {
        (Ok(()), Ok(_)) => Ok(()),
        (Ok(()), Err(e)) => Err(VmadmError::Seed(format!(
            "cidata written but `lofiadm -d {lofi}` failed (leaked device): {e}"
        ))),
        (Err(e), _) => Err(e),
    }
}

async fn seed_format_and_write(
    lofi: &str,
    mount_point: &str,
    files: &crate::seed::SeedFiles,
) -> Result<()> {
    let raw_lofi = lofi.replace("/lofi/", "/rlofi/");
    let sectors = crate::constants::NOCLOUD_SECTORS.to_string();
    let mkfs_opts = format!(
        "b={},nofdisk,fat={},size={sectors}",
        crate::constants::NOCLOUD_LABEL,
        crate::constants::NOCLOUD_DISK_SIZE_MIB
    );
    // mkfs_pcfs prompts "Construct a new file system?"; answer yes.
    run(
        MKFS,
        &["-F", "pcfs", "-o", &mkfs_opts, &raw_lofi],
        Some("y\n"),
        &[],
        &[],
    )
    .await?;
    run(
        MOUNT,
        &["-F", "pcfs", "-o", "rw", lofi, mount_point],
        None,
        &[],
        &[],
    )
    .await?;

    let write_result = write_seed_files(mount_point, files).await;

    // Unmount always; surface an unmount failure only when the write itself
    // succeeded (a still-mounted FAT image is a leak, not a no-op).
    let umount = run(UMOUNT, &[mount_point], None, &[], &[]).await;
    match (write_result, umount) {
        (Ok(()), Ok(_)) => Ok(()),
        (Ok(()), Err(e)) => Err(VmadmError::Seed(format!(
            "cidata written but `umount {mount_point}` failed (leaked mount): {e}"
        ))),
        (Err(e), _) => Err(e),
    }
}

async fn write_seed_files(mount_point: &str, files: &crate::seed::SeedFiles) -> Result<()> {
    let dir = std::path::Path::new(mount_point);
    write_seed_file(&dir.join("meta-data"), &files.meta_data).await?;
    if let Some(nc) = &files.network_config {
        write_seed_file(&dir.join("network-config"), nc).await?;
    }
    write_seed_file(&dir.join("user-data"), &files.user_data).await?;
    if let Some(vd) = &files.vendor_data {
        write_seed_file(&dir.join("vendor-data"), vd).await?;
    }
    Ok(())
}

async fn write_seed_file(path: &std::path::Path, contents: &str) -> Result<()> {
    tokio::fs::write(path, contents.as_bytes())
        .await
        .map_err(|e| VmadmError::Io {
            path: path.to_path_buf(),
            source: e,
        })
}

/// Whether `link` is a Proteus dataplane pseudo-link (`proteus<N>`, N>=1).
/// `proteus0` is rejected: node `VM.js` `isProteusLinkNicTag` requires a
/// non-zero index, and a zero low-32 port id should never reach here.
fn is_proteus_link(link: &str) -> bool {
    match link.strip_prefix("proteus") {
        Some(n) => !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()) && n != "0",
        None => false,
    }
}

#[async_trait]
impl NicTagRegistrar for HostNicTagRegistrar {
    async fn ensure(&self, link: &str) -> Result<()> {
        // Proteus dataplane pseudo-links (`proteusN`) are NOT nic_tags and
        // must never be registered as etherstubs. jcommon resolves them via
        // the patched brand hook (`dladm show-link` -> `misc`), not via
        // nictagadm's conf, so a registration never helped a zone boot.
        // Worse, the persisted `etherstub=proteusN` line makes the platform
        // recreate the OPTE port as a real etherstub on the next boot,
        // corrupting /usbkey across reboots. bhyve attaches the raw link via
        // viona regardless. No-op for both brands.
        if is_proteus_link(link) {
            return Ok(());
        }

        if nictag_exists(link).await? {
            return Ok(());
        }

        // A missing ordinary tag (admin/external/...) is an operator/config
        // error, not something to silently materialize as an empty etherstub.
        // Fail closed; the joyent brand statechange hook would bail at boot.
        Err(VmadmError::Payload(format!(
            "nic_tag {link:?} is not registered and is not a Proteus link; refusing to auto-create"
        )))
    }
}

/// `nictagadm exists -l <link>` — exit 0 means registered.
async fn nictag_exists(link: &str) -> Result<bool> {
    let status = Command::new(NICTAGADM)
        .args(["exists", "-l", link])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .await
        .map_err(|e| VmadmError::Spawn {
            bin: NICTAGADM.to_string(),
            source: e,
        })?;
    Ok(status.success())
}

#[cfg(test)]
mod tests {
    use super::{HostNicTagRegistrar, is_proteus_link, stderr_tolerated};
    use crate::backend::traits::NicTagRegistrar;

    // M6: tolerate matching is line-anchored, not substring-anywhere.
    #[test]
    fn stderr_tolerated_anchoring() {
        // node's exact already-halted phrase, trimmed + line-final, matches.
        assert!(stderr_tolerated(
            "zoneadm: zone is already halted\n",
            &["zone is already halted"]
        ));
        // Whole-stderr trim also matches (no trailing newline).
        assert!(stderr_tolerated(
            "zone is already halted",
            &["zone is already halted"]
        ));
        // A phrase that is only a mid-line substring must NOT be tolerated:
        // the old `contains` would have swallowed this unrelated failure.
        assert!(!stderr_tolerated(
            "zone is already halted but the dataset is busy",
            &["zone is already halted"]
        ));
        // Empty tolerate list never matches.
        assert!(!stderr_tolerated("anything", &[]));
        // A phrase on its own line within multi-line stderr matches.
        assert!(stderr_tolerated(
            "warning: noise\ndataset does not exist\n",
            &["does not exist"]
        ));
    }

    #[test]
    fn proteus_link_recognition() {
        assert!(is_proteus_link("proteus49377"));
        assert!(is_proteus_link("proteus1"));
        // proteus0 is rejected (zero index).
        assert!(!is_proteus_link("proteus0"));
        // ordinary tags are not auto-creatable.
        assert!(!is_proteus_link("admin"));
        assert!(!is_proteus_link("external"));
        assert!(!is_proteus_link("proteus"));
        assert!(!is_proteus_link("proteusX"));
    }

    // #36: a proteus pseudo-link must be a no-op — no nictagadm spawn, no
    // etherstub conf write — so a reboot leaves no stale `etherstub=proteusN`
    // in /usbkey. The early return means this never shells out to nictagadm,
    // which is also why the test runs on a non-illumos host.
    #[tokio::test]
    async fn proteus_links_are_never_registered() {
        let reg = HostNicTagRegistrar::new();
        reg.ensure("proteus49377")
            .await
            .expect("proteus link must be a no-op, not a registration");
        reg.ensure("proteus1")
            .await
            .expect("proteus link must be a no-op, not a registration");
    }
}
