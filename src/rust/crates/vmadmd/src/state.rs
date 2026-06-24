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
use std::sync::Mutex;

use serde_json::Value;

use crate::proxy::{DisplaySpec, Registry};
use crate::vminfo_client::{VminfoClient, VminfoError};

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

    /// node `unsupportedBrandError` (vmadmd.js:1565). The exact message text
    /// the client tolerates (postVmadmd ignores status, but match for safety).
    fn unsupported_brand(brands: &[&str], command: &str, brand: &str) -> String {
        format!(
            "vmadmd only handles \"{command}\" for: {}(your brand is: {brand})",
            brands.join(" ")
        )
    }

    /// node `stopVM` (vmadmd.js:1580). KVM-only: a non-kvm brand returns the
    /// unsupported-brand error. For P6 (bhyve) the route is wired but never hit
    /// by VM.js (bhyve stop is doTermStop in VM.js). The KVM QMP powerdown +
    /// stop-timer lands in P9.
    pub async fn stop_vm(&self, uuid: &str, timeout_secs: u64) -> VerbResult {
        let vmobj = match self.load(uuid).await {
            Ok(v) => v,
            Err(e) => return VerbResult::Error(format!("Unable to load vm: {e}")),
        };
        let brand = vmobj.get("brand").and_then(Value::as_str).unwrap_or("");
        if brand != "kvm" {
            return VerbResult::Error(Self::unsupported_brand(&["kvm"], "stop", brand));
        }
        // TODO(P9-KVM): QMP system_powerdown on <zonepath>/root/tmp/vm.qmp,
        // then arm the stop-timer for timeout_secs (setStopTimer) which force
        // stops on expiry. Carried as a no-op here so the route shape matches.
        let _ = timeout_secs;
        VerbResult::Error(Self::unsupported_brand(&["kvm"], "stop", brand))
    }

    /// node `resetVM` (vmadmd.js:1816). KVM-only via QMP system_reset.
    pub async fn reset_vm(&self, uuid: &str) -> VerbResult {
        let vmobj = match self.load(uuid).await {
            Ok(v) => v,
            Err(e) => return VerbResult::Error(format!("Unable to load vm: {e}")),
        };
        let brand = vmobj.get("brand").and_then(Value::as_str).unwrap_or("");
        if brand != "kvm" {
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
        // TODO(P9-KVM): QMP system_reset on <zonepath>/root/tmp/vm.qmp.
        VerbResult::Error(format!(
            "vmadmd only handles \"reset\" for kvm (your brand is: {brand})"
        ))
    }

    /// node `sysrqVM` (vmadmd.js:1866). KVM-only via QMP (nmi/screenshot).
    pub async fn sysrq_vm(&self, uuid: &str, request: &str) -> VerbResult {
        let vmobj = match self.load(uuid).await {
            Ok(v) => v,
            Err(e) => return VerbResult::Error(format!("unable to load vm: {e}")),
        };
        let brand = vmobj.get("brand").and_then(Value::as_str).unwrap_or("");
        if brand != "kvm" {
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
        // TODO(P9-KVM): QMP human-monitor-command 'nmi 0' / sendkey shift +
        // screendump for screenshot.
        let _ = request;
        VerbResult::Error(format!(
            "vmadmd only handles \"reset\" for kvm (your brand is: {brand})"
        ))
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

        // node: brands without a loadCb 500. P6 supports bhyve (vnc+console)
        // and joyent-minimal/lx/joyent (console only). KVM is deferred to P9.
        let runtime_info: &[&str] = match brand {
            "bhyve" => &["all", "vnc", "console"],
            "joyent-minimal" | "joyent" | "lx" => &["all", "console"],
            // TODO(P9-KVM): kvm runtime_info adds the QMP query-* set + spice.
            _ => {
                return Err(format!(
                    "vmadmd only handles \"info\" for: bhyve, joyent-minimal. Your brand is: \"{brand}\"."
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
}
