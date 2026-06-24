// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.
//
// Copyright 2026 Edgecast Cloud LLC.

//! Build the create-time `zonecfg` command stream from a [`VmCreateSpec`],
//! reproducing node `VM.js` `createZone` + `buildZonecfgUpdate` ordering.
//! Pure: no host calls.

use crate::constants as k;
use crate::model::nic::{NicAddr, ruin_mac};
use crate::model::spec::VmCreateSpec;
use crate::zonecfg::doc::Zonecfg;

/// Render the full `zonecfg -z <zone>` stdin blob for a create.
/// `now` provides the `create-timestamp` and the transition expiry, so
/// the output is deterministic for tests.
#[must_use]
pub fn build_create_zonecfg(spec: &VmCreateSpec, now: chrono::DateTime<chrono::Utc>) -> String {
    let mut z = Zonecfg::new();

    // ---- immutable header (createZone:9737) ----
    z.line("create -b");
    z.set("zonepath", &spec.zonepath);
    z.set("brand", spec.brand.as_str());
    z.set("uuid", &spec.uuid.to_string());
    z.line("set ip-type=exclusive");
    z.attr_provisioning();
    z.attr("vm-version", &spec.vm_version.to_string());
    z.attr("create-timestamp", &iso8601(now));

    // ---- provisioning transition (only when booting) ----
    if spec.autoboot {
        let expire_ms = now.timestamp_millis() + provision_timeout_ms(spec);
        z.attr_transition("provisioning", "running", expire_ms);
    }

    // dataset-uuid: emitted whenever the payload carried image_uuid
    // (OS zoneroot image; bhyve usually omits it).
    if let Some(image) = spec.image_uuid {
        z.attr("dataset-uuid", &image.to_string());
    }

    // (use_vm_autoboot is false for both supported brands, so the header
    // `set autoboot=false` is skipped; autoboot is emitted below instead.)

    // ---- buildZonecfgUpdate ----
    emit_global_props(&mut z, spec);
    // `set autoboot=<bool>` (use_vm_autoboot false for both brands).
    z.set("autoboot", &spec.autoboot.to_string());
    z.capped_memory(
        spec.max_physical_memory_mib,
        spec.max_locked_memory_mib,
        spec.max_swap_mib,
    );
    if let Some(cap) = spec.cpu_cap
        && cap > 0
    {
        z.capped_cpu(cap);
    }
    emit_attrs(&mut z, spec);
    if spec.brand.is_hvm() {
        emit_disks(&mut z, spec);
    }
    emit_nics(&mut z, spec);

    z.render()
}

/// `propertyMap` order from `buildZonecfgUpdate`: cpu-shares, limitpriv
/// (quoted), max-lwps, the System-V IPC limits, zfs-io-priority.
/// `max-shm-memory` is given to zonecfg in bytes (`formatMaxShmMemory`
/// multiplies the MiB value by 1 MiB).
fn emit_global_props(z: &mut Zonecfg, spec: &VmCreateSpec) {
    z.set("cpu-shares", &spec.cpu_shares.to_string());
    z.set("limitpriv", &format!("\"{}\"", spec.limit_priv.join(",")));
    z.set("max-lwps", &spec.max_lwps.to_string());
    z.set("max-msg-ids", &spec.max_msg_ids.to_string());
    z.set("max-sem-ids", &spec.max_sem_ids.to_string());
    z.set("max-shm-ids", &spec.max_shm_ids.to_string());
    // bytes = MiB * 1 MiB; normalize rejects values that would overflow, so
    // saturating_mul here is just belt-and-suspenders against a panic.
    z.set(
        "max-shm-memory",
        &spec.max_shm_memory_mib.saturating_mul(1024 * 1024).to_string(),
    );
    z.set("zfs-io-priority", &spec.zfs_io_priority.to_string());
}

/// The `setAttr(...)` sequence in `buildZonecfgUpdate` order. Each attr is
/// emitted only when present (mirroring node's `payload.hasOwnProperty`
/// plus the empty-value drop for a new machine).
fn emit_attrs(z: &mut Zonecfg, spec: &VmCreateSpec) {
    // Always present (applyZoneDefaults stamps both).
    z.attr("billing-id", &spec.billing_id.to_string());
    z.attr("owner-uuid", &spec.owner_uuid.to_string());
    if let Some(tmpfs) = spec.tmpfs_mib {
        z.attr("tmpfs", &tmpfs.to_string());
    }
    if let Some(hostname) = &spec.hostname {
        z.attr("hostname", hostname);
    }
    z.attr("dns-domain", &spec.dns_domain);
    if spec.do_not_inventory {
        z.attr("do-not-inventory", "true");
    }
    if spec.firewall_enabled {
        z.attr("firewall-enabled", "true");
    }
    if spec.maintain_resolvers {
        z.attr("maintain-resolvers", "true");
    }
    if !spec.resolvers.is_empty() {
        z.attr("resolvers", &spec.resolvers.join(","));
    }
    if let Some(alias) = &spec.alias {
        z.attr_base64("alias", alias);
    }

    if spec.brand.is_hvm() {
        z.attr("ram", &spec.ram_mib.to_string());
        // com1/com2 default to the KVM-compatible values when unset
        // (buildZonecfgUpdate bhyve block).
        z.attr("com1", "/dev/zconsole");
        z.attr("com2", "socket,/tmp/vm.ttyb");
        if let Some(bootrom) = &spec.bootrom {
            z.attr("bootrom", bootrom);
        }
        if let Some(opts) = &spec.bhyve_extra_opts {
            z.attr("bhyve-extra-opts", opts);
        }
        z.attr("zlog-mode", "g--");
        z.attr("zlog-name", "platform.log");
        z.attr("vcpus", &spec.vcpus.to_string());
        // buildZonecfgUpdate emits boot then vnc-port in the HVM attr block
        // (VM.js:7009/7012); KVM-only attrs (cpu-type/vga/spice/virtio-*)
        // are intentionally not ported.
        if let Some(boot) = &spec.boot {
            z.attr("boot", boot);
        }
        if let Some(vnc) = spec.vnc_port {
            z.attr("vnc-port", &vnc.to_string());
        }
        if let Some(flex) = spec.flexible_disk_size_mib {
            z.attr("flexible-disk-size", &flex.to_string());
        }
    }
}

/// `buildDiskZonecfg` (bhyve only).
fn emit_disks(z: &mut Zonecfg, spec: &VmCreateSpec) {
    for d in &spec.disks {
        let mut b = String::new();
        b.push_str("add device\n");
        b.push_str(&format!("set match={}\n", d.raw_path));
        b.push_str(&format!("add property (name=boot, value=\"{}\")\n", d.boot));
        b.push_str(&format!(
            "add property (name=model, value=\"{}\")\n",
            d.model.as_str()
        ));
        if let Some(media) = d.media {
            b.push_str(&format!(
                "add property (name=media, value=\"{}\")\n",
                media.as_str()
            ));
        }
        if let Some(sz) = d.image_size_mib {
            b.push_str(&format!("add property (name=image-size, value=\"{sz}\")\n"));
        } else if let Some(sz) = d.size_mib {
            b.push_str(&format!("add property (name=size, value=\"{sz}\")\n"));
        }
        if let Some(img) = d.image_uuid {
            b.push_str(&format!(
                "add property (name=image-uuid, value=\"{img}\")\n"
            ));
        }
        if let Some(slot) = &d.pci_slot {
            b.push_str(&format!("add property (name=pci-slot, value=\"{slot}\")\n"));
        }
        b.push_str(&format!("add property (name=uuid, value=\"{}\")\n", d.uuid));
        b.push_str("end\n");
        z.push_block(&b);
    }
}

/// `buildNicZonecfg`: the per-NIC `add net` blocks then the post-loop
/// `select net ...; ... primary ...; end` lines.
fn emit_nics(z: &mut Zonecfg, spec: &VmCreateSpec) {
    for nic in &spec.nics {
        let mut b = String::new();
        b.push_str("add net\n");
        b.push_str(&format!("set physical={}\n", nic.interface));
        b.push_str(&format!("set mac-addr={}\n", ruin_mac(&nic.mac)));
        if let Some(tag) = &nic.nic_tag {
            b.push_str(&format!("set global-nic={tag}\n"));
        }

        // gateways (modern form; legacy `gateway` was upgraded in normalize):
        // node emits the representative v4 as `gateway` plus the full
        // `gateways` list.
        if !nic.gateways.is_empty() {
            if let Some(g4) = nic.gateways.iter().find(|g| g.is_ipv4()) {
                b.push_str(&format!("add property (name=gateway, value=\"{g4}\")\n"));
            }
            let joined = nic
                .gateways
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
                .join(",");
            b.push_str(&format!(
                "add property (name=gateways, value=\"{joined}\")\n"
            ));
        }

        // ips (modern form): representative v4 -> netmask + ip, then the
        // full ips list.
        if !nic.ips.is_empty() {
            if let Some(ipnetwork::IpNetwork::V4(v4)) = NicAddr::representative_ipv4(&nic.ips) {
                b.push_str(&format!(
                    "add property (name=netmask, value=\"{}\")\n",
                    v4.mask()
                ));
                b.push_str(&format!("add property (name=ip, value=\"{}\")\n", v4.ip()));
            }
            let joined = nic
                .ips
                .iter()
                .map(NicAddr::as_string)
                .collect::<Vec<_>>()
                .join(",");
            b.push_str(&format!("add property (name=ips, value=\"{joined}\")\n"));
        }

        if let Some(vlan) = nic.vlan_id {
            b.push_str(&format!("set vlan-id={vlan}\n"));
        }

        // Anti-spoof allow-list (a security policy: emit it, never drop it).
        if !nic.allowed_ips.is_empty() {
            let joined = nic
                .allowed_ips
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
                .join(",");
            b.push_str(&format!(
                "add property (name=allowed_ips, value=\"{joined}\")\n"
            ));
        }

        // nicProperties order: model, allow_dhcp_spoofing, allow_ip_spoofing,
        // allow_mac_spoofing, allow_restricted_traffic, allow_unfiltered_promisc,
        // mtu (only those the payload set).
        if let Some(model) = &nic.model {
            b.push_str(&format!("add property (name=model, value=\"{model}\")\n"));
        }
        emit_bool_prop(&mut b, "allow_dhcp_spoofing", nic.allow_dhcp_spoofing);
        emit_bool_prop(&mut b, "allow_ip_spoofing", nic.allow_ip_spoofing);
        emit_bool_prop(&mut b, "allow_mac_spoofing", nic.allow_mac_spoofing);
        emit_bool_prop(
            &mut b,
            "allow_restricted_traffic",
            nic.allow_restricted_traffic,
        );
        emit_bool_prop(
            &mut b,
            "allow_unfiltered_promisc",
            nic.allow_unfiltered_promisc,
        );
        if let Some(mtu) = nic.mtu {
            b.push_str(&format!("add property (name=mtu, value=\"{mtu}\")\n"));
        }

        b.push_str("end\n");
        z.push_block(&b);
    }

    // Post-loop primary handling: the first primary NIC is the new
    // primary; node emits a select line for every NIC.
    if let Some(primary) = spec.nics.iter().find(|n| n.primary) {
        let primary_mac = ruin_mac(&primary.mac);
        for nic in &spec.nics {
            let mac = ruin_mac(&nic.mac);
            if mac == primary_mac {
                z.line(&format!(
                    "select net mac-addr={mac}; add property (name=primary,value=\"true\"); end"
                ));
            } else {
                z.line(&format!(
                    "select net mac-addr={mac}; remove -F property (name=primary,value=\"true\"); end"
                ));
            }
        }
    }
}

fn emit_bool_prop(b: &mut String, name: &str, value: Option<bool>) {
    if let Some(v) = value {
        b.push_str(&format!("add property (name={name}, value=\"{v}\")\n"));
    }
}

/// `provision_timeout * 1000`, with the bhyve ram-based multiplier
/// (`createZone:9763`).
fn provision_timeout_ms(spec: &VmCreateSpec) -> i64 {
    let mut secs = k::PROVISION_TIMEOUT_SECS;
    if spec.brand.is_hvm() && spec.ram_mib > 0 {
        let mult = (spec.ram_mib / 4096).saturating_sub(1);
        if mult > 0 {
            secs += mult * secs;
        }
    }
    (secs * 1000) as i64
}

/// ISO-8601 with millisecond precision and a `Z`, matching JS
/// `Date.toISOString()` (`2026-06-20T18:00:00.000Z`).
fn iso8601(now: chrono::DateTime<chrono::Utc>) -> String {
    now.format("%Y-%m-%dT%H:%M:%S%.3fZ").to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::payload::{VmadmCreatePayload, normalize};

    fn now() -> chrono::DateTime<chrono::Utc> {
        chrono::DateTime::from_timestamp(0, 0).unwrap_or_default()
    }

    fn render(json: serde_json::Value) -> String {
        let p = VmadmCreatePayload::from_json_slice(json.to_string().as_bytes())
            .unwrap_or_else(|e| panic!("payload: {e}"));
        let spec = normalize(p, now()).unwrap_or_else(|e| panic!("normalize: {e}"));
        build_create_zonecfg(&spec, now())
    }

    #[test]
    fn bhyve_limitpriv_lockdown_emitted() {
        // H1: the quoted limitpriv carries the six-element bhyve lockdown.
        let out = render(serde_json::json!({"brand": "bhyve", "ram": 512}));
        assert!(out.contains(
            "set limitpriv=\"default,-file_link_any,-net_access,-proc_fork,-proc_info,-proc_session\""
        ));
    }

    #[test]
    fn vnc_port_and_boot_attrs_emitted_for_bhyve() {
        // M4: boot then vnc-port appear in the HVM attr block.
        let out = render(serde_json::json!({
            "brand": "bhyve", "ram": 512, "vnc_port": 5900, "boot": "order=cd"
        }));
        assert!(out.contains("set name=\"boot\"; set type=string; set value=\"order=cd\""));
        assert!(out.contains("set name=\"vnc-port\"; set type=string; set value=\"5900\""));
    }

    #[test]
    fn no_vnc_or_boot_attr_when_absent() {
        let out = render(serde_json::json!({"brand": "bhyve", "ram": 512}));
        assert!(!out.contains("name=\"vnc-port\""));
        assert!(!out.contains("name=\"boot\""));
    }

    #[test]
    fn max_shm_memory_emitted_in_bytes() {
        let out = render(serde_json::json!({
            "brand": "joyent-minimal", "max_physical_memory": 1, "max_shm_memory": 1
        }));
        assert!(out.contains("set max-shm-memory=1048576"));
    }
}
