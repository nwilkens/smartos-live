// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.
//
// Copyright 2026 Edgecast Cloud LLC.

//! The node-vmadm-compatible create payload — the engine's single create
//! input. The drop-in `vmadm` binary deserializes it from stdin; the
//! agent builds the same JSON in-process. Fields the engine does not
//! model yet are tolerated and round-tripped via `extra` so a forward
//! payload never fails to parse.

use serde::Deserialize;
use serde_json::Map;
use uuid::Uuid;

pub type JsonMap = Map<String, serde_json::Value>;

#[derive(Debug, Clone, Deserialize)]
pub struct VmadmCreatePayload {
    #[serde(default)]
    pub uuid: Option<Uuid>,
    #[serde(default)]
    pub zonename: Option<String>,
    pub brand: String,
    #[serde(default)]
    pub image_uuid: Option<Uuid>,
    /// lx kernel ABI version (`kernel-version` attr). Required on an lx
    /// create (proptable.js:1058), ignored for other brands.
    #[serde(default)]
    pub kernel_version: Option<String>,
    #[serde(default)]
    pub alias: Option<String>,
    #[serde(default)]
    pub hostname: Option<String>,
    #[serde(default)]
    pub dns_domain: Option<String>,
    #[serde(default)]
    pub owner_uuid: Option<Uuid>,
    #[serde(default)]
    pub billing_id: Option<Uuid>,
    #[serde(default)]
    pub autoboot: Option<bool>,
    #[serde(default)]
    pub zpool: Option<String>,

    // sizing
    #[serde(default)]
    pub quota: Option<u64>, // GiB
    #[serde(default)]
    pub cpu_cap: Option<u32>,
    #[serde(default)]
    pub cpu_shares: Option<u32>,
    #[serde(default)]
    pub zfs_io_priority: Option<u32>,
    #[serde(default)]
    pub max_lwps: Option<u32>,
    #[serde(default)]
    pub max_msg_ids: Option<u32>,
    #[serde(default)]
    pub max_sem_ids: Option<u32>,
    #[serde(default)]
    pub max_shm_ids: Option<u32>,
    #[serde(default)]
    pub max_shm_memory: Option<u64>, // MiB
    #[serde(default)]
    pub max_physical_memory: Option<u64>, // MiB
    #[serde(default)]
    pub max_locked_memory: Option<u64>,
    #[serde(default)]
    pub max_swap: Option<u64>,
    #[serde(default)]
    pub tmpfs: Option<u64>,
    #[serde(default)]
    pub ram: Option<u64>, // bhyve MiB
    #[serde(default)]
    pub vcpus: Option<u32>,
    #[serde(default)]
    pub bootrom: Option<String>,
    #[serde(default)]
    pub flexible_disk_size: Option<u64>, // MiB
    /// HVM VNC TCP port (`vnc-port` attr). 0 = random, -1 = disabled.
    #[serde(default)]
    pub vnc_port: Option<i64>,
    /// HVM boot order string passed to the brand (`boot` attr).
    #[serde(default)]
    pub boot: Option<String>,

    // networking / identity
    #[serde(default)]
    pub resolvers: Option<Vec<String>>,
    #[serde(default)]
    pub routes: Option<JsonMap>,
    #[serde(default)]
    pub maintain_resolvers: Option<bool>,
    #[serde(default)]
    pub firewall_enabled: Option<bool>,
    #[serde(default)]
    pub do_not_inventory: Option<bool>,
    #[serde(default)]
    pub delegate_dataset: Option<bool>,
    #[serde(default)]
    pub indestructible_zoneroot: Option<bool>,
    #[serde(default)]
    pub indestructible_delegated: Option<bool>,
    #[serde(default)]
    pub limit_priv: Option<serde_json::Value>, // string (csv) or array
    #[serde(default)]
    pub mdata_exec_timeout: Option<u64>,

    #[serde(default)]
    pub customer_metadata: Option<JsonMap>,
    #[serde(default)]
    pub internal_metadata: Option<JsonMap>,
    #[serde(default)]
    pub tags: Option<JsonMap>,

    // docker (proptable.js:798). A docker zone is an lx/joyent-minimal zone
    // with docker=true plus `docker:*` internal_metadata (which flows in via
    // `internal_metadata` above). The create path derives the rest
    // (VM.js:10085-10148). `init_name`/`restart_init` are also accepted
    // directly by node (proptable.js:1006/1842); an explicit value wins over
    // the docker-derived default.
    #[serde(default)]
    pub docker: Option<bool>,
    #[serde(default)]
    pub init_name: Option<String>,
    #[serde(default)]
    pub restart_init: Option<bool>,

    #[serde(default, alias = "nics")]
    pub add_nics: Option<Vec<NicPayload>>,
    #[serde(default, alias = "disks")]
    pub add_disks: Option<Vec<DiskPayload>>,

    #[serde(default)]
    pub bhyve_extra_opts: Option<String>,
    #[serde(default)]
    pub create_only: Option<bool>,
    #[serde(default)]
    pub nowait: Option<bool>,

    // Accepted by node but not yet ported here. We keep the fields so a
    // create that sets them is rejected (L10/L11) rather than silently
    // dropping behaviour an operator may rely on.
    #[serde(default)]
    pub archive_on_delete: Option<bool>,
    #[serde(default)]
    pub internal_metadata_namespaces: Option<Vec<String>>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct NicPayload {
    #[serde(default)]
    pub mac: Option<String>,
    #[serde(default, alias = "physical")]
    pub interface: Option<String>,
    #[serde(default)]
    pub nic_tag: Option<String>,
    #[serde(default)]
    pub vlan_id: Option<u16>,
    /// Modern CIDR form (`["10.0.0.5/24", "dhcp", "addrconf"]`).
    #[serde(default)]
    pub ips: Option<Vec<String>>,
    /// Legacy form.
    #[serde(default)]
    pub ip: Option<String>,
    #[serde(default)]
    pub netmask: Option<String>,
    #[serde(default)]
    pub gateways: Option<Vec<String>>,
    #[serde(default)]
    pub gateway: Option<String>,
    #[serde(default)]
    pub primary: Option<bool>,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub mtu: Option<u32>,
    #[serde(default)]
    pub allow_ip_spoofing: Option<bool>,
    #[serde(default)]
    pub allow_mac_spoofing: Option<bool>,
    #[serde(default)]
    pub allow_dhcp_spoofing: Option<bool>,
    #[serde(default)]
    pub allow_restricted_traffic: Option<bool>,
    #[serde(default)]
    pub allow_unfiltered_promisc: Option<bool>,
    #[serde(default)]
    pub allowed_ips: Option<Vec<String>>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct DiskPayload {
    #[serde(default)]
    pub uuid: Option<Uuid>,
    #[serde(default)]
    pub path: Option<String>,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub media: Option<String>,
    #[serde(default)]
    pub boot: Option<bool>,
    #[serde(default)]
    pub size: Option<u64>, // MiB
    #[serde(default)]
    pub image_uuid: Option<Uuid>,
    #[serde(default)]
    pub image_size: Option<u64>, // MiB
    #[serde(default)]
    pub compression: Option<String>,
    #[serde(default)]
    pub block_size: Option<u64>,
    #[serde(default)]
    pub refreservation: Option<u64>,
    #[serde(default)]
    pub pci_slot: Option<String>,
    #[serde(default)]
    pub nocreate: Option<bool>,
}

impl VmadmCreatePayload {
    /// Parse a create payload from the node-vmadm JSON byte stream.
    pub fn from_json_slice(bytes: &[u8]) -> Result<Self, serde_json::Error> {
        serde_json::from_slice(bytes)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_minimal_bhyve_payload() {
        let json = serde_json::json!({
            "uuid": "11111111-1111-1111-1111-111111111111",
            "brand": "bhyve",
            "ram": 2048,
            "vcpus": 2,
            "flexible_disk_size": 20480,
            "disks": [
                { "boot": true, "model": "virtio", "image_uuid":
                  "22222222-2222-2222-2222-222222222222", "size": 20480 }
            ],
            "nics": [
                { "interface": "net0", "nic_tag": "external",
                  "ips": ["10.0.0.5/24"], "gateways": ["10.0.0.1"],
                  "mac": "02:00:00:de:ad:01", "primary": true }
            ]
        });
        let p = VmadmCreatePayload::from_json_slice(json.to_string().as_bytes()).unwrap();
        assert_eq!(p.brand, "bhyve");
        assert_eq!(p.ram, Some(2048));
        // `disks`/`nics` aliases populate add_disks/add_nics.
        assert_eq!(p.add_disks.as_ref().unwrap().len(), 1);
        assert_eq!(
            p.add_nics.as_ref().unwrap()[0].nic_tag.as_deref(),
            Some("external")
        );
    }

    #[test]
    fn tolerates_unknown_fields() {
        let json = serde_json::json!({
            "brand": "joyent-minimal",
            "some_future_field": { "a": 1 },
            "image_uuid": "22222222-2222-2222-2222-222222222222"
        });
        let p = VmadmCreatePayload::from_json_slice(json.to_string().as_bytes()).unwrap();
        assert_eq!(p.brand, "joyent-minimal");
    }
}
