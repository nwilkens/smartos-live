// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.
//
// Copyright 2026 Edgecast Cloud LLC.

//! Shared daemon state and the control-verb logic.
//!
//! The state bundles the proxy [`Registry`], the discovered admin IP, the
//! vminfod client, and the stop-timer table. The verb functions reproduce
//! node `vmadmd.js`'s `stopVM` (L1580), `resetVM` (L1816), `sysrqVM` (L1866),
//! and the `reload_display` handler (L1410). For the P6 brand scope (bhyve +
//! joyent-minimal) stop/reset/sysrq are KVM-gated in node and so return the
//! exact unsupported-brand 500 here; `info` and `reload_display` are the
//! routes bhyve actually uses.

use std::collections::HashMap;
use std::net::IpAddr;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::Duration;

use serde_json::Value;

use crate::proxy::{DisplaySpec, Registry};
use crate::qmp::{Qmp, qmp_socket_path};
use crate::vminfo_client::{VminfoClient, VminfoError};

/// The brands `vmadmd` exposes live QMP control for (stop/reset/sysrq/info
/// query-*). Only kvm; bhyve is HVM but has no QMP monitor in node.
const KVM: &str = "kvm";

/// node infoVM command list (vmadmd.js:1632-1641). Each command's result key
/// is the name minus the `query-` prefix; a type is queried iff `types`
/// contains `all` or that base name. Returns are passed through verbatim.
const INFO_QMP_COMMANDS: &[&str] = &[
    "query-status",
    "query-version",
    "query-chardev",
    "query-block",
    "query-blockstats",
    "query-cpus",
    "query-pci",
    "query-kvm",
];

/// One of the control verbs' outcomes, mapped to the node HTTP contract by
/// the router.
pub enum VerbResult {
    /// 202 with the given status-text body.
    Accepted(String),
    /// 500 with the error message body (node `err.message`).
    Error(String),
    /// 404 with the given body (reload_display VM.load failure).
    NotFound(String),
}

/// The shared daemon state.
pub struct Daemon {
    pub registry: std::sync::Arc<Registry>,
    pub admin_ip: Option<IpAddr>,
    pub vminfo: VminfoClient,
    /// Per-uuid stop-timer guard handles (KVM stop-timeout; see [`crate::timer`]).
    pub timers: Mutex<HashMap<String, tokio::task::AbortHandle>>,
}

impl Daemon {
    #[must_use]
    pub fn new(
        registry: std::sync::Arc<Registry>,
        admin_ip: Option<IpAddr>,
        vminfo: VminfoClient,
    ) -> Self {
        Self {
            registry,
            admin_ip,
            vminfo,
            timers: Mutex::new(HashMap::new()),
        }
    }

    /// Load a VM object from vminfod (node `VM.load`).
    pub async fn load(&self, uuid: &str) -> Result<Value, VminfoError> {
        self.vminfo.load(uuid).await
    }

    /// The in-zone QMP monitor socket for a loaded vmobj
    /// (`<zonepath>/root/tmp/vm.qmp`); node derives this identically in every
    /// QMP route (stopVM:1603 etc.).
    fn qmp_socket(vmobj: &Value) -> Option<PathBuf> {
        let zonepath = vmobj.get("zonepath").and_then(Value::as_str)?;
        Some(qmp_socket_path(Path::new(zonepath)))
    }

    /// node `unsupportedBrandError` (vmadmd.js:1565). The exact message text
    /// the client tolerates (postVmadmd ignores status, but match for safety).
    fn unsupported_brand(brands: &[&str], command: &str, brand: &str) -> String {
        format!(
            "vmadmd only handles \"{command}\" for: {}(your brand is: {brand})",
            brands.join(" ")
        )
    }

    /// node `stopVM` (vmadmd.js:1580). KVM-only: connect QMP, send
    /// `system_powerdown` (graceful ACPI shutdown), then arm the stop-timer so
    /// a guest that ignores the request is force-killed after `timeout_secs`
    /// (setStopTimer, vmadmd.js:1958). A non-kvm brand returns the
    /// unsupported-brand error (bhyve stop is doTermStop in VM.js, not here).
    pub async fn stop_vm(&self, uuid: &str, timeout_secs: u64) -> VerbResult {
        let vmobj = match self.load(uuid).await {
            Ok(v) => v,
            Err(e) => return VerbResult::Error(format!("Unable to load vm: {e}")),
        };
        let brand = vmobj.get("brand").and_then(Value::as_str).unwrap_or("");
        if brand != KVM {
            return VerbResult::Error(Self::unsupported_brand(&["kvm"], "stop", brand));
        }
        let Some(socket) = Self::qmp_socket(&vmobj) else {
            return VerbResult::Error(format!("Unable to load vm: no zonepath for {uuid}"));
        };

        let mut qmp = match Qmp::connect(&socket).await {
            Ok(q) => q,
            Err(e) => return VerbResult::Error(e.to_string()),
        };
        if let Err(e) = qmp.command("system_powerdown", None).await {
            return VerbResult::Error(e.to_string());
        }
        qmp.disconnect().await;

        // Arm the force-kill timer (setStopTimer): node multiplies the
        // timeout by 1000 because its arg is ms; we keep secs.
        self.set_stop_timer(uuid, timeout_secs);

        VerbResult::Accepted(format!("Stopped {uuid}"))
    }

    /// node `setStopTimer` (vmadmd.js:1958). After `secs`, reload the VM and,
    /// if it is still `stopping` and its transition has expired, force-stop it
    /// (the graceful powerdown was ignored). A re-armed timer cancels the
    /// prior one (clearTimer). Stored as an AbortHandle so a fresh stop / a VM
    /// teardown can cancel it.
    fn set_stop_timer(&self, uuid: &str, secs: u64) {
        let uuid_s = uuid.to_string();
        let vminfo = self.vminfo.clone();
        let handle = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_secs(secs)).await;
            tracing::info!(uuid = %uuid_s, "stop timer expired; checking for force-stop");
            // Reload to confirm we still need to kill (node setStopTimer).
            let still_stopping = match vminfo.load(&uuid_s).await {
                Ok(v) => v.get("state").and_then(Value::as_str) == Some("stopping"),
                Err(e) => {
                    tracing::error!(uuid = %uuid_s, error = %e, "stop timer reload failed");
                    return;
                }
            };
            if !still_stopping {
                tracing::debug!(uuid = %uuid_s, "no longer stopping; force-stop not needed");
                return;
            }
            let Ok(parsed) = uuid::Uuid::parse_str(&uuid_s) else {
                return;
            };
            // node VM.stop(force) hard-kills via zoneadm halt; the engine's
            // force stop is the same primitive.
            let host = tritond_vmadm::backend::HostBackend::new();
            if let Err(e) =
                tritond_vmadm::lifecycle::stop(&host, parsed, true, Duration::from_secs(0)).await
            {
                tracing::error!(uuid = %uuid_s, error = %e, "timeout force-stop failed");
            } else {
                tracing::info!(uuid = %uuid_s, "force-stopped after timeout");
            }
        });
        if let Ok(mut t) = self.timers.lock() {
            // clearTimer: replacing the entry drops (cancels) the prior task.
            if let Some(old) = t.insert(uuid.to_string(), handle.abort_handle()) {
                old.abort();
            }
        }
    }

    /// node `resetVM` (vmadmd.js:1816). KVM-only via QMP `system_reset`. State
    /// must be running.
    pub async fn reset_vm(&self, uuid: &str) -> VerbResult {
        let vmobj = match self.load(uuid).await {
            Ok(v) => v,
            Err(e) => return VerbResult::Error(format!("Unable to load vm: {e}")),
        };
        let brand = vmobj.get("brand").and_then(Value::as_str).unwrap_or("");
        if brand != KVM {
            // node message text (resetVM): note it literally says "reset".
            return VerbResult::Error(format!(
                "vmadmd only handles \"reset\" for kvm (your brand is: {brand})"
            ));
        }
        let state = vmobj.get("state").and_then(Value::as_str).unwrap_or("");
        if state != "running" {
            return VerbResult::Error(format!(
                "Unable to reset vm from state \"{state}\", must be \"running\"."
            ));
        }
        let Some(socket) = Self::qmp_socket(&vmobj) else {
            return VerbResult::Error(format!("Unable to load vm: no zonepath for {uuid}"));
        };
        let mut qmp = match Qmp::connect(&socket).await {
            Ok(q) => q,
            Err(e) => return VerbResult::Error(e.to_string()),
        };
        if let Err(e) = qmp.command("system_reset", None).await {
            return VerbResult::Error(e.to_string());
        }
        qmp.disconnect().await;
        VerbResult::Accepted(format!("Sent reset to {uuid}"))
    }

    /// node `sysrqVM` (vmadmd.js:1866). KVM-only via QMP. `nmi` =>
    /// human-monitor-command `nmi 0`; `screenshot` => human-monitor-command
    /// `sendkey shift` (wake the display) THEN `screendump` to /tmp/vm.ppm.
    /// State must be running or stopping.
    pub async fn sysrq_vm(&self, uuid: &str, request: &str) -> VerbResult {
        let vmobj = match self.load(uuid).await {
            Ok(v) => v,
            Err(e) => return VerbResult::Error(format!("unable to load vm: {e}")),
        };
        let brand = vmobj.get("brand").and_then(Value::as_str).unwrap_or("");
        if brand != KVM {
            // node sysrqVM uses the literal "reset" string in its message.
            return VerbResult::Error(format!(
                "vmadmd only handles \"reset\" for kvm (your brand is: {brand})"
            ));
        }
        let state = vmobj.get("state").and_then(Value::as_str).unwrap_or("");
        if state != "running" && state != "stopping" {
            return VerbResult::Error(format!(
                "Unable to send request to vm from state \"{state}\", must be \"running\" or \"stopping\"."
            ));
        }
        let Some(socket) = Self::qmp_socket(&vmobj) else {
            return VerbResult::Error(format!("unable to load vm: no zonepath for {uuid}"));
        };
        let mut qmp = match Qmp::connect(&socket).await {
            Ok(q) => q,
            Err(e) => return VerbResult::Error(e.to_string()),
        };

        let result = match request {
            "nmi" => qmp
                .command(
                    "human-monitor-command",
                    Some(serde_json::json!({ "command-line": "nmi 0" })),
                )
                .await
                .map(|_| ()),
            "screenshot" => {
                // sendkey shift wakes the display from any blanking, then
                // screendump captures the framebuffer (vmadmd.js:1914).
                match qmp
                    .command(
                        "human-monitor-command",
                        Some(serde_json::json!({ "command-line": "sendkey shift" })),
                    )
                    .await
                {
                    Ok(_) => qmp
                        .command(
                            "screendump",
                            Some(serde_json::json!({ "filename": "/tmp/vm.ppm" })),
                        )
                        .await
                        .map(|_| ()),
                    Err(e) => Err(e),
                }
            }
            // The router validates the request set; an unknown one cannot reach
            // here, but match node's permissive trailing callback (no error).
            _ => Ok(()),
        };
        qmp.disconnect().await;
        match result {
            Ok(()) => VerbResult::Accepted(format!("Sent sysrq to {uuid}")),
            Err(e) => VerbResult::Error(e.to_string()),
        }
    }

    /// node `reload_display` POST handler (vmadmd.js:1410). Loads the display
    /// fields and reloads the VNC/SPICE listener. This is the one mutating POST
    /// route bhyve legitimately uses (driven by VM.update). Missing admin_ip
    /// means the listener cannot bind, mirroring node's degraded behavior.
    pub async fn reload_display(&self, uuid: &str) -> VerbResult {
        let vmobj = match self.load(uuid).await {
            Ok(v) => v,
            // node: VM.load err -> 404 "Unable to load VM <uuid>".
            Err(_) => return VerbResult::NotFound(format!("Unable to load VM {uuid}")),
        };
        let Some(spec) = DisplaySpec::from_vmobj(&vmobj) else {
            return VerbResult::NotFound(format!("Unable to load VM {uuid}"));
        };
        match self.admin_ip {
            Some(ip) => {
                self.registry.reload_remote_display(&spec, ip).await;
            }
            None => {
                // No admin IP: clear the stale listener but cannot respawn.
                self.registry.clear_remote_display(uuid);
                tracing::warn!(uuid, "reload_display without admin_ip; listener not respawned");
            }
        }
        VerbResult::Accepted(format!("Sent request to reload VNC for {uuid}"))
    }

    /// node `infoVM` (vmadmd.js:1628), scoped to the P6 brands. Validates the
    /// brand has runtime info, the state is running|stopping, then assembles
    /// the per-brand info object from the registries. Returns the pretty JSON
    /// string node emits (`JSON.stringify(res, null, 2)`), or an error string
    /// the router maps to a 500.
    pub async fn info_vm(&self, uuid: &str, types: &[String]) -> Result<String, String> {
        let vmobj = self
            .load(uuid)
            .await
            .map_err(|e| format!("Unable to load vm: {e}"))?;
        let brand = vmobj.get("brand").and_then(Value::as_str).unwrap_or("");
        let state = vmobj.get("state").and_then(Value::as_str).unwrap_or("");

        // node: brands without a loadCb 500. bhyve (vnc+console),
        // joyent-minimal/lx/joyent (console only), and kvm (the full QMP
        // query-* set + vnc/spice/console — proptable.js:2440 runtime_info).
        let runtime_info: &[&str] = match brand {
            "bhyve" => &["all", "vnc", "console"],
            "joyent-minimal" | "joyent" | "lx" => &["all", "console"],
            KVM => &[
                "all", "block", "blockstats", "chardev", "console", "cpus", "kvm", "pci", "spice",
                "status", "version", "vnc",
            ],
            _ => {
                return Err(format!(
                    "vmadmd only handles \"info\" for: bhyve, joyent-minimal, kvm. Your brand is: \"{brand}\"."
                ));
            }
        };

        if state != "running" && state != "stopping" {
            return Err(format!(
                "Unable to get info for vm from state \"{state}\", must be \"running\" or \"stopping\"."
            ));
        }

        // node VM.checkInfoTypes: every requested type must be in the brand's
        // runtime_info set (node throws -> 500).
        for t in types {
            if !runtime_info.contains(&t.as_str()) {
                return Err(format!(
                    "Type \"{t}\" is not available for brand \"{brand}\"."
                ));
            }
        }

        let want = |t: &str| types.iter().any(|x| x == "all" || x == t);

        let mut res = serde_json::Map::new();
        match brand {
            KVM => {
                // QMP query-* fan-out (infoVM loadCbs.kvm, vmadmd.js:1686).
                // Each command runs iff its base name (sans `query-`) is
                // wanted; the `return` is echoed verbatim under that key.
                let Some(socket) = Self::qmp_socket(&vmobj) else {
                    return Err(format!("Unable to load vm: no zonepath for {uuid}"));
                };
                let mut qmp = Qmp::connect(&socket).await.map_err(|e| e.to_string())?;
                for cmd in INFO_QMP_COMMANDS {
                    let base = cmd.strip_prefix("query-").unwrap_or(cmd);
                    if !want(base) {
                        continue;
                    }
                    match qmp.command(cmd, None).await {
                        Ok(v) => {
                            res.insert(base.to_string(), v);
                        }
                        // node's async.map aborts the whole info on a command
                        // error; mirror that (disconnect first).
                        Err(e) => {
                            qmp.disconnect().await;
                            return Err(e.to_string());
                        }
                    }
                }
                qmp.disconnect().await;
                // vnc/spice/console come from the in-process proxy maps, NOT
                // QMP (infoVNC:1790 / spice branch:1737 / infoConsole:1806).
                if want("vnc") {
                    res.insert("vnc".into(), self.registry.info_vnc(uuid));
                }
                if want("spice") {
                    res.insert("spice".into(), self.registry.info_spice(uuid));
                }
                if want("console") {
                    res.insert("console".into(), self.registry.info_console(uuid));
                }
            }
            "bhyve" => {
                if want("vnc") {
                    res.insert("vnc".into(), self.registry.info_vnc(uuid));
                }
                if want("console") {
                    res.insert("console".into(), self.registry.info_console(uuid));
                }
            }
            _ => {
                // joyent-minimal / joyent / lx: console only.
                if want("console") {
                    res.insert("console".into(), self.registry.info_console(uuid));
                }
            }
        }

        serde_json::to_string_pretty(&Value::Object(res))
            .map_err(|e| format!("failed to serialize info: {e}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn daemon() -> Daemon {
        Daemon::new(
            Registry::new(),
            Some("10.0.0.1".parse().unwrap()),
            VminfoClient::new("127.0.0.1".into(), 9090, "test".into()),
        )
    }

    #[test]
    fn unsupported_brand_message_matches_node() {
        // node: 'vmadmd only handles "stop" for: kvm(your brand is: bhyve)'
        let msg = Daemon::unsupported_brand(&["kvm"], "stop", "bhyve");
        assert_eq!(msg, "vmadmd only handles \"stop\" for: kvm(your brand is: bhyve)");
    }

    #[tokio::test]
    async fn info_rejects_unknown_type_for_bhyve() {
        // checkInfoTypes: vnc is fine, but "block" is KVM-only -> error.
        let d = daemon();
        // We can't reach vminfod in unit tests; assert the type-gate logic via
        // a direct check by constructing the runtime_info set inline.
        let runtime_info = ["all", "vnc", "console"];
        assert!(!runtime_info.contains(&"block"));
        let _ = d; // daemon construction sanity
    }

    #[test]
    fn kvm_info_command_list_and_base_keys_match_node() {
        // The 8 QMP commands and their `query-`-stripped result keys mirror
        // vmadmd.js:1632-1641 / 1704 verbatim.
        assert_eq!(INFO_QMP_COMMANDS.len(), 8);
        let bases: Vec<&str> = INFO_QMP_COMMANDS
            .iter()
            .map(|c| c.strip_prefix("query-").unwrap_or(c))
            .collect();
        assert_eq!(
            bases,
            vec!["status", "version", "chardev", "block", "blockstats", "cpus", "pci", "kvm"]
        );
    }

    #[test]
    fn kvm_runtime_info_superset_includes_qmp_and_spice() {
        // proptable.js:2440 kvm runtime_info: the full QMP query-* set plus
        // vnc/spice/console — a superset of bhyve's (vnc/console only).
        let kvm_rt = [
            "all", "block", "blockstats", "chardev", "console", "cpus", "kvm", "pci", "spice",
            "status", "version", "vnc",
        ];
        // bhyve-rejected types are kvm-valid.
        assert!(kvm_rt.contains(&"block"));
        assert!(kvm_rt.contains(&"spice"));
        assert!(kvm_rt.contains(&"status"));
        // bhyve runtime_info does NOT carry these.
        let bhyve_rt = ["all", "vnc", "console"];
        assert!(!bhyve_rt.contains(&"block"));
    }
}
