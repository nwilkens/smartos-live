// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.
//
// Copyright 2026 Edgecast Cloud LLC.

//! The startup / lifecycle responsibilities beyond the HTTP control plane:
//! autoboot, provisioning-marker unset, and log rotation.
//!
//! These reproduce node `vmadmd.js` `main`/`loadVM` autoboot (L2605/L2007),
//! `handleProvisioning` (L660), and `rotateKVMLog` (L791), scoped to the P6
//! brands (bhyve + joyent-minimal).

use std::net::IpAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use serde_json::Value;
use uuid::Uuid;

use crate::proxy::{ConsoleSpec, DisplaySpec, Registry};
use crate::state::Daemon;

/// node `VMADMD_AUTOBOOT_FILE` (vmadmd.js:64). Written once per GZ boot so a
/// vmadmd restart (vs a host reboot) does not re-autoboot VMs.
pub const AUTOBOOT_MARKER: &str = "/tmp/.autoboot_vmadmd";

/// The result of the autoboot scan: which VMs would be started, and the marker
/// decision. Returned so callers (and tests) can assert the logic without
/// actually booting anything.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct AutobootScan {
    /// True when the marker was absent (this is a fresh GZ boot).
    pub do_autoboot: bool,
    /// uuids that should be `VM.start`ed (autoboot=true and do_autoboot, or
    /// never_booted).
    pub to_start: Vec<String>,
    /// Running bhyve/kvm VMs whose display+console proxies should be spawned.
    pub to_spawn_display: Vec<String>,
    /// Running non-HVM VMs whose console proxy should be spawned.
    pub to_spawn_console_only: Vec<String>,
    /// VMs in 'provisioning' state needing the provisioning-marker handled.
    pub provisioning: Vec<String>,
}

/// Decide the autoboot/spawn plan from the vminfod VM list and the marker
/// presence. Pure function (no side effects) so it is unit-testable and so the
/// VERIFY step can run it against the live zone list without booting anything.
///
/// node `main`/`finishUpgrade`/`loadVM`: per VM,
/// * provisioning -> handleProvisioning;
/// * bhyve/kvm -> loadVM (start if never_booted || (autoboot && do_autoboot);
///   always spawn display+console);
/// * non-HVM running -> spawn console only.
#[must_use]
pub fn plan_autoboot(vms: &[Value], marker_present: bool) -> AutobootScan {
    let do_autoboot = !marker_present;
    let mut scan = AutobootScan {
        do_autoboot,
        ..Default::default()
    };

    for vm in vms {
        let Some(uuid) = vm.get("uuid").and_then(Value::as_str) else {
            continue;
        };
        let brand = vm.get("brand").and_then(Value::as_str).unwrap_or("");
        let state = vm.get("state").and_then(Value::as_str).unwrap_or("");
        let zone_state = vm.get("zone_state").and_then(Value::as_str).unwrap_or("");
        let autoboot = vm.get("autoboot").and_then(Value::as_bool).unwrap_or(false);
        let never_booted = vm
            .get("never_booted")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let is_hvm = brand == "bhyve" || brand == "kvm";
        let running = state == "running" || zone_state == "running";

        if state == "failed" {
            continue;
        }
        if state == "provisioning" {
            scan.provisioning.push(uuid.to_string());
            continue;
        }
        if is_hvm {
            if never_booted || (autoboot && do_autoboot) {
                scan.to_start.push(uuid.to_string());
            }
            scan.to_spawn_display.push(uuid.to_string());
        } else if running {
            scan.to_spawn_console_only.push(uuid.to_string());
        }
    }

    scan
}

/// Ensure the autoboot marker exists, returning whether it was ABSENT before
/// (i.e. whether this is a fresh boot => do_autoboot). node writes 'booted'.
pub fn take_autoboot_marker(path: &Path) -> bool {
    let absent = !path.exists();
    if absent
        && let Err(e) = std::fs::write(path, "booted")
    {
        tracing::warn!(path = %path.display(), error = %e, "failed to write autoboot marker");
    }
    absent
}

/// Run the autoboot plan against the host: start VMs and spawn proxies. Uses
/// the cn-platform zoneadm tool for the actual boot (node uses VM.start, which
/// ultimately drives zoneadm boot). The display/console specs come from the
/// vminfod object.
pub async fn run_autoboot(
    daemon: &Arc<Daemon>,
    registry: &Arc<Registry>,
    admin_ip: Option<IpAddr>,
    vms: &[Value],
    marker_present: bool,
) {
    let plan = plan_autoboot(vms, marker_present);
    tracing::info!(
        do_autoboot = plan.do_autoboot,
        starts = plan.to_start.len(),
        displays = plan.to_spawn_display.len(),
        consoles = plan.to_spawn_console_only.len(),
        provisioning = plan.provisioning.len(),
        "autoboot scan"
    );

    let zoneadm = tritond_cn_platform::smartos::ZoneadmTool::new();
    for uuid in &plan.to_start {
        if let Ok(parsed) = Uuid::parse_str(uuid) {
            // node loadVM -> VM.start; we boot via zoneadm (idempotent).
            match zoneadm.lookup(uuid).await {
                Ok(info) if !info.is_running() => {
                    boot_zone(&zoneadm, parsed).await;
                }
                Ok(_) => {}
                Err(e) => tracing::warn!(uuid, error = %e, "autoboot lookup failed"),
            }
        }
    }

    // Map uuid -> vmobj for spec building.
    for uuid in &plan.to_spawn_display {
        if let Some(vm) = find_vm(vms, uuid) {
            if let (Some(spec), Some(ip)) = (DisplaySpec::from_vmobj(vm), admin_ip) {
                registry.spawn_remote_display(&spec, ip).await;
            }
            if let (Some(cspec), Some(ip)) = (ConsoleSpec::from_vmobj(vm), admin_ip) {
                registry.spawn_console_proxy(&cspec, ip).await;
            }
            // node rotateKVMLog is KVM-only; bhyve has no vmadmd-driven
            // rotation (see rotate_log). P6 brand scope: skip for bhyve.
            if vm.get("brand").and_then(Value::as_str) == Some("kvm") {
                rotate_kvm_log(uuid);
            }
        }
    }
    for uuid in &plan.to_spawn_console_only {
        if let Some(vm) = find_vm(vms, uuid)
            && let (Some(cspec), Some(ip)) = (ConsoleSpec::from_vmobj(vm), admin_ip)
        {
            registry.spawn_console_proxy(&cspec, ip).await;
        }
    }

    for uuid in &plan.provisioning {
        if let Some(vm) = find_vm(vms, uuid) {
            handle_provisioning(daemon, registry, admin_ip, vm).await;
        }
    }
}

async fn boot_zone(zoneadm: &tritond_cn_platform::smartos::ZoneadmTool, uuid: Uuid) {
    // node loadVM -> VM.start; drive the engine's start (idempotent), which
    // halts/boots via the same zoneadm primitive. node ignores VM.start
    // errors during autoboot, so we log and continue.
    let _ = zoneadm; // caller already gated on the live not-running state.
    let host = tritond_vmadm::backend::HostBackend::new();
    if let Err(e) = tritond_vmadm::lifecycle::start(&host, uuid).await {
        tracing::warn!(uuid = %uuid, error = %e, "autoboot: VM.start failed");
    }
}

fn find_vm<'a>(vms: &'a [Value], uuid: &str) -> Option<&'a Value> {
    vms.iter()
        .find(|v| v.get("uuid").and_then(Value::as_str) == Some(uuid))
}

/// node `handleProvisioning` (vmadmd.js:660). Inspects the in-zone
/// `/var/svc/provisioning|provision_success|provision_failure` markers, unsets
/// the provisioning transition, and (on success, for HVM) spawns the display +
/// console proxies. The transition-unset itself is the vmadm engine's job
/// (VM.unsetTransition); here we drive the marker-driven decision and the
/// post-success proxy spawn, which is the vmadmd-owned part.
pub async fn handle_provisioning(
    daemon: &Arc<Daemon>,
    registry: &Arc<Registry>,
    admin_ip: Option<IpAddr>,
    vm: &Value,
) {
    let Some(uuid) = vm.get("uuid").and_then(Value::as_str) else {
        return;
    };
    let zonepath = vm
        .get("zonepath")
        .and_then(Value::as_str)
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(format!("/zones/{uuid}")));

    let provisioning = zonepath.join("root/var/svc/provisioning");
    let success = zonepath.join("root/var/svc/provision_success");
    let failure = zonepath.join("root/var/svc/provision_failure");

    let outcome = if provisioning.exists() {
        // node waits for /var/svc/provisioning to disappear; the engine
        // (VM.waitForProvisioning) owns the wait. Treat presence as "still
        // provisioning" and let the next zone-event/scan re-evaluate.
        tracing::debug!(uuid, "provisioning marker still present; deferring");
        ProvisionOutcome::Pending
    } else if success.exists() {
        tracing::info!(uuid, "provision_success seen");
        ProvisionOutcome::Success
    } else if failure.exists() {
        tracing::warn!(uuid, "provision_failure seen; marking VM failed");
        ProvisionOutcome::Failure
    } else {
        tracing::warn!(uuid, "no provisioning markers; assuming success");
        ProvisionOutcome::Success
    };

    // TODO(wiring): on Success/Failure call the vmadm engine's
    // unsetTransition / markVMFailure. The proxy spawn below is the
    // vmadmd-owned post-success step (node success(), L668).
    if outcome == ProvisionOutcome::Success {
        // Reload the VM and, if running, clear + spawn display/console.
        if let Ok(fresh) = daemon.load(uuid).await {
            let running = fresh.get("state").and_then(Value::as_str) == Some("running")
                || fresh.get("zone_state").and_then(Value::as_str) == Some("running");
            let brand = fresh.get("brand").and_then(Value::as_str).unwrap_or("");
            if running && (brand == "bhyve" || brand == "kvm") {
                registry.clear_vm(uuid);
                if let (Some(spec), Some(ip)) = (DisplaySpec::from_vmobj(&fresh), admin_ip) {
                    registry.spawn_remote_display(&spec, ip).await;
                }
                if let (Some(cspec), Some(ip)) = (ConsoleSpec::from_vmobj(&fresh), admin_ip) {
                    registry.spawn_console_proxy(&cspec, ip).await;
                }
            }
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
enum ProvisionOutcome {
    Pending,
    Success,
    Failure,
}

/// node `rotateKVMLog` (vmadmd.js:791): 30s after a KVM VM goes running, run
/// `/usr/vm/sbin/rotate-kvm-logs.sh <uuid>` so a rapid reboot loop does not
/// lose the boot log. KVM-only; bhyve has no vmadmd-driven rotation in node.
pub fn rotate_kvm_log(uuid: &str) {
    let uuid = uuid.to_string();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_secs(30)).await;
        let out = tokio::process::Command::new("/usr/vm/sbin/rotate-kvm-logs.sh")
            .arg(&uuid)
            .output()
            .await;
        match out {
            Ok(o) if o.status.success() => {
                tracing::debug!(uuid, "rotated kvm log for just-booted VM");
            }
            Ok(o) => tracing::error!(
                uuid,
                stderr = %String::from_utf8_lossy(&o.stderr),
                "failed to rotate kvm log"
            ),
            Err(e) => tracing::error!(uuid, error = %e, "failed to spawn rotate-kvm-logs.sh"),
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn vm(uuid: &str, brand: &str, state: &str, autoboot: bool) -> Value {
        json!({
            "uuid": uuid,
            "brand": brand,
            "state": state,
            "zone_state": state,
            "autoboot": autoboot
        })
    }

    #[test]
    fn fresh_boot_starts_autoboot_hvm() {
        let vms = vec![
            vm("a", "bhyve", "stopped", true),
            vm("b", "bhyve", "stopped", false),
        ];
        let scan = plan_autoboot(&vms, false);
        assert!(scan.do_autoboot);
        assert_eq!(scan.to_start, vec!["a"]);
        // both HVM get display/console spawn attempts regardless of autoboot.
        assert_eq!(scan.to_spawn_display, vec!["a", "b"]);
    }

    #[test]
    fn restart_does_not_reautoboot() {
        let vms = vec![vm("a", "bhyve", "stopped", true)];
        let scan = plan_autoboot(&vms, true);
        assert!(!scan.do_autoboot);
        assert!(scan.to_start.is_empty());
    }

    #[test]
    fn never_booted_starts_even_on_restart() {
        let mut v = vm("a", "bhyve", "stopped", false);
        v["never_booted"] = json!(true);
        let scan = plan_autoboot(&[v], true);
        assert_eq!(scan.to_start, vec!["a"]);
    }

    #[test]
    fn non_hvm_running_gets_console_only() {
        let vms = vec![vm("z", "joyent-minimal", "running", true)];
        let scan = plan_autoboot(&vms, false);
        assert!(scan.to_spawn_display.is_empty());
        assert_eq!(scan.to_spawn_console_only, vec!["z"]);
        // non-HVM is NOT autobooted by vmadmd (node only loadVMs bhyve/kvm).
        assert!(scan.to_start.is_empty());
    }

    #[test]
    fn failed_vm_skipped() {
        let vms = vec![vm("f", "bhyve", "failed", true)];
        let scan = plan_autoboot(&vms, false);
        assert!(scan.to_start.is_empty());
        assert!(scan.to_spawn_display.is_empty());
    }

    #[test]
    fn provisioning_vm_routed() {
        let vms = vec![vm("p", "bhyve", "provisioning", true)];
        let scan = plan_autoboot(&vms, false);
        assert_eq!(scan.provisioning, vec!["p"]);
        assert!(scan.to_spawn_display.is_empty());
    }

    #[test]
    fn marker_take_is_idempotent() {
        let dir = tempfile::tempdir().expect("tmp");
        let marker = dir.path().join(".autoboot_vmadmd");
        assert!(take_autoboot_marker(&marker), "first call: absent");
        assert!(!take_autoboot_marker(&marker), "second call: present");
    }
}
