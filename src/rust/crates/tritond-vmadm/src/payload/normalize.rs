// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.
//
// Copyright 2026 Edgecast Cloud LLC.

//! `normalizePayload` + `applyZoneDefaults` (node `VM.js`): turn a
//! [`VmadmCreatePayload`] into the brand-agnostic [`VmCreateSpec`] the
//! create algorithm consumes. Defaults, memory derivation, NIC/disk
//! normalization, and MAC generation all happen here.

use std::net::{IpAddr, Ipv4Addr};

use ipnetwork::IpNetwork;
use serde_json::Value;
use uuid::Uuid;

use crate::brand::Brand;
use crate::constants as k;
use crate::error::VmadmError;
use crate::model::disk::{DiskMedia, DiskModel, DiskSpec, Refreservation};
use crate::model::nic::{NicAddr, NicSpec};
use crate::model::spec::{JsonMap, VmCreateSpec};
use crate::payload::compat::{DiskPayload, NicPayload, VmadmCreatePayload};

/// Normalize a create payload, stamping `create_timestamp` from the
/// supplied clock so callers (and tests) control time.
pub fn normalize(
    payload: VmadmCreatePayload,
    now: chrono::DateTime<chrono::Utc>,
) -> Result<VmCreateSpec, VmadmError> {
    let brand = Brand::parse(&payload.brand)?;
    // L10/L11: node honors these on create but this engine does not yet
    // port them. Fail closed rather than silently dropping the behaviour.
    if payload.archive_on_delete == Some(true) {
        return Err(VmadmError::Payload(
            "archive_on_delete is not yet supported by this vmadm".to_string(),
        ));
    }
    if payload
        .internal_metadata_namespaces
        .as_ref()
        .is_some_and(|n| !n.is_empty())
    {
        return Err(VmadmError::Payload(
            "internal_metadata_namespaces are not yet supported by this vmadm".to_string(),
        ));
    }
    let uuid = payload.uuid.unwrap_or_else(Uuid::new_v4);

    // Compute memory while `payload` is still fully owned (derive_memory
    // borrows it); only after this do we move fields out by value.
    let ram_mib = payload.ram.unwrap_or(k::DEFAULT_RAM_MIB);
    let mem = derive_memory(brand, &payload, ram_mib);

    let zonename = payload.zonename.unwrap_or_else(|| uuid.to_string());
    let zpool = payload
        .zpool
        .unwrap_or_else(|| k::DEFAULT_ZPOOL.to_string());
    let zfs_filesystem = format!("{zpool}/{uuid}");
    let zonepath = format!("/{zfs_filesystem}");

    let quota_gib = payload.quota.unwrap_or(match brand {
        // Only bhyve uses the RFD-0148 refquota-on-zoneroot model whose
        // default is DEFAULT_BHYVE_QUOTA (1 GiB). kvm's zvols are siblings
        // outside the zoneroot, so the zoneroot gets the plain DEFAULT_QUOTA
        // (10 GiB) like an OS zone (setQuota else-branch, VM.js:1465).
        Brand::Bhyve => k::DEFAULT_BHYVE_QUOTA_GIB,
        Brand::Kvm | Brand::JoyentMinimal | Brand::Lx => k::DEFAULT_QUOTA_GIB,
    });

    // kernel_version: required for lx (proptable.js:1058 required: ['create'])
    // and meaningless for the other brands. Fail closed on an lx create that
    // omits it rather than emitting an lx zone the brand cannot install.
    let kernel_version = match brand {
        Brand::Lx => Some(payload.kernel_version.clone().ok_or_else(|| {
            VmadmError::Payload("lx create requires kernel_version".to_string())
        })?),
        _ => None,
    };

    // tmpfs default (VM.js:4925 applyZoneDefaults): OS brands mount a
    // tmpfs /tmp sized to max_physical_memory. When the payload omits
    // tmpfs (or asks for more than max_physical_memory) node clamps it to
    // max_physical_memory; HVM brands never get a tmpfs attr.
    let tmpfs_mib = if brand.features().use_tmpfs {
        match payload.tmpfs {
            Some(t) if t <= mem.physical => Some(t),
            _ => Some(mem.physical),
        }
    } else {
        payload.tmpfs
    };

    let nics = normalize_nics(uuid, payload.add_nics.unwrap_or_default())?;
    let mut disks = if brand.is_hvm() {
        normalize_disks(&zpool, uuid, brand, payload.add_disks.unwrap_or_default())?
    } else {
        Vec::new()
    };

    // bhyve images are applied to the boot disk (disk0), NOT the zoneroot:
    // node treats a bhyve image as a zvol cloned into disk0, never as a
    // zoneroot install template. A top-level `image_uuid` passed as the
    // `zoneadm install -t` template makes the brand try to clone it as the
    // zoneroot dataset and fails ("'devices' does not apply to datasets of
    // this type"). So for bhyve, push a top-level image_uuid down onto a
    // boot disk that lacks one, and drop it from the top level so it never
    // reaches the OS-zone install template.
    //
    // KVM is DIFFERENT: a top-level image_uuid on kvm is the SPICE-root
    // case — installZone keeps it as `-t image_uuid -x nodataset` so the
    // brand clones it as the (hidden) zoneroot (VM.js:8405). So for kvm we
    // leave a top-level image_uuid in place; the guest's bootable image is
    // normally carried on disk0's own image_uuid, which is unaffected here.
    let mut top_image_uuid = payload.image_uuid;
    if brand == Brand::Bhyve
        && let Some(img) = top_image_uuid
    {
        let boot = disks.iter_mut().find(|d| d.boot);
        if let Some(boot) = boot
            && boot.image_uuid.is_none()
        {
            boot.image_uuid = Some(img);
        }
        top_image_uuid = None;
    }

    let customer_metadata = payload.customer_metadata.unwrap_or_default();
    let mut internal_metadata = payload.internal_metadata.unwrap_or_default();
    let nocloud = detect_nocloud(&customer_metadata, &internal_metadata);

    // Insert the NoCloud cidata seed disk for bhyve guests and record its
    // path so it can be regenerated later (nocloud.js updatePayloadDisks).
    let mut flexible_disk_size_mib = payload.flexible_disk_size;
    if brand.is_hvm() && nocloud {
        let seed = nocloud_seed_disk(&zpool, uuid, disks.len() as u32, brand);
        internal_metadata.insert(
            "cloudinit_nocloud_path".to_string(),
            Value::String(seed.raw_path.clone()),
        );
        disks.push(seed);
        assign_pci_slots(&mut disks);
        // Bump the flex budget by the seed's 16 MiB ONLY when the guest
        // already uses flexible disks. Introducing a flex budget on a
        // non-flex guest switches the quota to the flex form (which
        // subtracts the full boot-disk volsize) and computes a zoneroot
        // quota smaller than the boot disk's own reservation.
        flexible_disk_size_mib = flexible_disk_size_mib.map(|f| f + k::NOCLOUD_DISK_SIZE_MIB);
    }

    let spec = VmCreateSpec {
        uuid,
        zonename,
        brand,
        zpool,
        zfs_filesystem,
        zonepath,
        image_uuid: top_image_uuid,
        kernel_version,
        create_timestamp: now,
        vm_version: 1,
        autoboot: payload.autoboot.unwrap_or(true),
        create_only: payload.create_only.unwrap_or(false),
        nowait: payload.nowait.unwrap_or(false),

        quota_gib,
        ram_mib,
        max_physical_memory_mib: mem.physical,
        max_locked_memory_mib: mem.locked,
        max_swap_mib: mem.swap,
        tmpfs_mib,
        vcpus: payload.vcpus.unwrap_or(1),
        cpu_cap: payload.cpu_cap,
        cpu_shares: payload.cpu_shares.unwrap_or(k::DEFAULT_CPU_SHARES),
        zfs_io_priority: payload
            .zfs_io_priority
            .unwrap_or(k::DEFAULT_ZFS_IO_PRIORITY),
        max_lwps: payload.max_lwps.unwrap_or(k::DEFAULT_MAX_LWPS),
        max_msg_ids: payload.max_msg_ids.unwrap_or(k::DEFAULT_MAX_MSG_IDS),
        max_sem_ids: payload.max_sem_ids.unwrap_or(k::DEFAULT_MAX_SEM_IDS),
        max_shm_ids: payload.max_shm_ids.unwrap_or(k::DEFAULT_MAX_SHM_IDS),
        // max_shm_memory defaults to max_physical_memory (MiB).
        max_shm_memory_mib: payload.max_shm_memory.unwrap_or(mem.physical),
        flexible_disk_size_mib,
        bootrom: payload.bootrom,
        // vnc_port/boot are bhyve HVM attrs; OS zones never carry them.
        vnc_port: if brand.is_hvm() { payload.vnc_port } else { None },
        boot: if brand.is_hvm() { payload.boot } else { None },

        limit_priv: parse_limit_priv(brand, payload.limit_priv.as_ref()),
        dns_domain: payload
            .dns_domain
            .unwrap_or_else(|| k::DEFAULT_DNS_DOMAIN.to_string()),
        hostname: payload.hostname,
        alias: payload.alias,
        owner_uuid: payload.owner_uuid.unwrap_or(k::ZERO_UUID),
        billing_id: payload.billing_id.unwrap_or(k::ZERO_UUID),
        resolvers: payload.resolvers.unwrap_or_default(),
        routes: payload.routes.unwrap_or_default(),
        firewall_enabled: payload.firewall_enabled.unwrap_or(false),
        maintain_resolvers: payload.maintain_resolvers.unwrap_or(false),
        do_not_inventory: payload.do_not_inventory.unwrap_or(false),
        delegate_dataset: payload.delegate_dataset.unwrap_or(false),
        indestructible_zoneroot: payload.indestructible_zoneroot.unwrap_or(false),
        indestructible_delegated: payload.indestructible_delegated.unwrap_or(false),
        mdata_exec_timeout_secs: payload
            .mdata_exec_timeout
            .unwrap_or(k::DEFAULT_MDATA_EXEC_TIMEOUT_SECS),

        customer_metadata,
        internal_metadata,
        tags: payload.tags.unwrap_or_default(),

        nics,
        disks,
        bhyve_extra_opts: payload.bhyve_extra_opts,
        nocloud,
    };
    validate_spec(&spec)?;
    Ok(spec)
}

struct Memory {
    physical: u64,
    locked: u64,
    swap: u64,
}

/// `fixPayloadMemory` (`VM.js:4620`). bhyve adds `default_memory_overhead`
/// to `ram` for `max_physical_memory` only when the payload omits an
/// explicit `max_physical_memory` (VM.js:4636); an explicit value is
/// honored verbatim. `max_locked`/`max_swap` cascade off the honored
/// physical value and are only defaulted when absent. On the auto path
/// `max_swap` is bumped to at least `MINIMUM_MAX_SWAP` (VM.js:4687); an
/// explicit `max_swap` is honored as-is.
fn derive_memory(brand: Brand, p: &VmadmCreatePayload, ram_mib: u64) -> Memory {
    let physical = match p.max_physical_memory {
        Some(mpm) => mpm,
        None => match brand {
            // HVM brands derive max_physical_memory = ram + overhead. The
            // overhead differs by brand (bhyve 1280, kvm 1024); both come
            // from features().default_memory_overhead_mib (VM.js:4636,
            // case 'kvm': v += KVM_MEM_OVERHEAD).
            Brand::Bhyve | Brand::Kvm => ram_mib + brand.features().default_memory_overhead_mib,
            // OS zones (joyent-minimal, lx): max_physical_memory defaults to
            // DEFAULT_RAM, no ram-based overhead.
            Brand::JoyentMinimal | Brand::Lx => k::DEFAULT_RAM_MIB,
        },
    };
    let locked = p.max_locked_memory.unwrap_or(physical);
    let swap = match p.max_swap {
        Some(s) => s,
        None => physical.max(k::MINIMUM_MAX_SWAP_MIB),
    };
    Memory {
        physical,
        locked,
        swap,
    }
}

/// `limit_priv` may arrive as a comma-separated string or an array. When
/// omitted, applyZoneDefaults stamps the brand's `features.limit_priv`
/// (bhyve locks the guest down; OS brands get `["default"]`) (VM.js:4932).
fn parse_limit_priv(brand: Brand, v: Option<&Value>) -> Vec<String> {
    match v {
        Some(Value::String(s)) => s.split(',').map(|t| t.trim().to_string()).collect(),
        Some(Value::Array(a)) => a
            .iter()
            .filter_map(|x| x.as_str().map(str::to_string))
            .collect(),
        _ => brand
            .features()
            .limit_priv
            .iter()
            .map(|p| (*p).to_string())
            .collect(),
    }
}

fn detect_nocloud(customer: &JsonMap, internal: &JsonMap) -> bool {
    let is_nocloud = |m: &JsonMap, key: &str| m.get(key).and_then(Value::as_str) == Some("nocloud");
    is_nocloud(internal, "cloudinit_datasource")
        || is_nocloud(customer, "org.smartos:cloudinit_datasource")
}

fn normalize_nics(vm_uuid: Uuid, raw: Vec<NicPayload>) -> Result<Vec<NicSpec>, VmadmError> {
    let any_primary = raw.iter().any(|n| n.primary.unwrap_or(false));
    raw.into_iter()
        .enumerate()
        .map(|(idx, n)| normalize_nic(vm_uuid, idx, n, any_primary))
        .collect()
}

fn normalize_nic(
    vm_uuid: Uuid,
    idx: usize,
    n: NicPayload,
    any_primary: bool,
) -> Result<NicSpec, VmadmError> {
    let interface = n.interface.unwrap_or_else(|| format!("net{idx}"));
    let mac = match n.mac {
        Some(m) => canonical_mac(&m)?,
        None => generate_mac(vm_uuid, idx),
    };

    // Upgrade the legacy `ip`/`netmask`/`gateway` form to the modern
    // `ips`/`gateways` form, exactly as node's nic.js `upgradeNic` does
    // before zonecfg generation (so the zone config is always modern).
    let mut ips = Vec::new();
    if let Some(list) = n.ips {
        for entry in list {
            ips.push(parse_nic_addr(&entry)?);
        }
    } else if let Some(legacy_ip) = n.ip.as_deref() {
        ips.push(upgrade_legacy_ip(legacy_ip, n.netmask.as_deref())?);
    }

    let mut gateways = match n.gateways {
        Some(list) => collect_gateways(Some(list), None)?,
        None => Vec::new(),
    };
    // Legacy `gateway` upgrades to a single-element `gateways`.
    if gateways.is_empty()
        && let Some(gw) = n.gateway.as_deref()
    {
        gateways.push(
            gw.parse::<IpAddr>()
                .map_err(|e| VmadmError::Payload(format!("bad gateway {gw}: {e}")))?,
        );
    }

    let allowed_ips = n
        .allowed_ips
        .unwrap_or_default()
        .iter()
        .map(|s| {
            s.parse::<IpNetwork>()
                .map_err(|e| VmadmError::Payload(format!("bad allowed_ip {s}: {e}")))
        })
        .collect::<Result<Vec<_>, _>>()?;

    // First NIC becomes primary when none was flagged (createZone:9725).
    let primary = n.primary.unwrap_or(false) || (!any_primary && idx == 0);

    Ok(NicSpec {
        mac,
        interface,
        nic_tag: n.nic_tag,
        vlan_id: n.vlan_id,
        ips,
        gateways,
        primary,
        model: n.model,
        mtu: n.mtu,
        allow_ip_spoofing: n.allow_ip_spoofing,
        allow_mac_spoofing: n.allow_mac_spoofing,
        allow_dhcp_spoofing: n.allow_dhcp_spoofing,
        allow_restricted_traffic: n.allow_restricted_traffic,
        allow_unfiltered_promisc: n.allow_unfiltered_promisc,
        allowed_ips,
    })
}

fn collect_gateways(
    list: Option<Vec<String>>,
    single: Option<String>,
) -> Result<Vec<IpAddr>, VmadmError> {
    let parse = |s: &str| {
        s.parse::<IpAddr>()
            .map_err(|e| VmadmError::Payload(format!("bad gateway {s}: {e}")))
    };
    if let Some(list) = list {
        list.iter().map(|s| parse(s)).collect()
    } else if let Some(g) = single {
        Ok(vec![parse(&g)?])
    } else {
        Ok(Vec::new())
    }
}

/// node nic.js `upgradeNic`: a legacy `ip` (+ dotted `netmask`) becomes a
/// single modern `ips` entry. `dhcp`/`addrconf` pass through as-is.
fn upgrade_legacy_ip(ip: &str, netmask: Option<&str>) -> Result<NicAddr, VmadmError> {
    match ip {
        "dhcp" => Ok(NicAddr::Dhcp),
        "addrconf" => Ok(NicAddr::Addrconf),
        addr => {
            let netmask = netmask.ok_or_else(|| {
                VmadmError::Payload(format!("legacy NIC ip {addr} has no netmask"))
            })?;
            let prefix = netmask_to_prefix(netmask)?;
            format!("{addr}/{prefix}")
                .parse::<IpNetwork>()
                .map(NicAddr::Net)
                .map_err(|e| VmadmError::Payload(format!("bad legacy ip {addr}/{netmask}: {e}")))
        }
    }
}

fn parse_nic_addr(entry: &str) -> Result<NicAddr, VmadmError> {
    match entry {
        "dhcp" => Ok(NicAddr::Dhcp),
        "addrconf" => Ok(NicAddr::Addrconf),
        cidr => cidr
            .parse::<IpNetwork>()
            .map(NicAddr::Net)
            .map_err(|e| VmadmError::Payload(format!("bad nic ip {cidr}: {e}"))),
    }
}

/// Canonicalize a MAC to lowercase, validating six hex octets.
fn canonical_mac(mac: &str) -> Result<String, VmadmError> {
    let octets: Vec<&str> = mac.split(':').collect();
    if octets.len() != 6 {
        return Err(VmadmError::Payload(format!("bad MAC {mac:?}")));
    }
    let mut out = Vec::with_capacity(6);
    for o in octets {
        let v = u8::from_str_radix(o, 16)
            .map_err(|_| VmadmError::Payload(format!("bad MAC octet {o:?} in {mac:?}")))?;
        out.push(format!("{v:02x}"));
    }
    Ok(out.join(":"))
}

/// Deterministically derive a locally-administered MAC from the VM uuid +
/// NIC index when the payload omits one. (node-vmadm generates a random
/// MAC; we derive a stable unique one — uniqueness is guaranteed by the
/// VM uuid and a real provisioner always supplies the MAC anyway.)
fn generate_mac(vm_uuid: Uuid, idx: usize) -> String {
    let b = vm_uuid.as_bytes();
    // 02 = locally administered, unicast. Mix in the index so a
    // payload with several MAC-less NICs stays unique.
    let i = idx as u8;
    format!(
        "02:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}",
        b[0] ^ i,
        b[5],
        b[10],
        b[14],
        b[15] ^ i
    )
}

fn normalize_disks(
    zpool: &str,
    vm_uuid: Uuid,
    brand: Brand,
    raw: Vec<DiskPayload>,
) -> Result<Vec<DiskSpec>, VmadmError> {
    let mut disks: Vec<DiskSpec> = raw
        .into_iter()
        .enumerate()
        .map(|(idx, d)| normalize_disk(zpool, vm_uuid, brand, idx as u32, d))
        .collect::<Result<_, _>>()?;
    assign_pci_slots(&mut disks);
    Ok(disks)
}

/// The 16 MiB NoCloud cidata seed disk (a blank virtio zvol the host
/// formats FAT16 `CIDATA`). PCI slot is assigned by [`assign_pci_slots`].
/// The dataset layout follows the brand (sibling for kvm, child for bhyve).
fn nocloud_seed_disk(zpool: &str, vm_uuid: Uuid, idx: u32, brand: Brand) -> DiskSpec {
    let zfs_filesystem = crate::model::disk::disk_dataset(zpool, vm_uuid, idx, brand);
    let raw_path = format!("/dev/zvol/rdsk/{zfs_filesystem}");
    DiskSpec {
        uuid: Uuid::new_v4(),
        zfs_filesystem,
        raw_path,
        disk_idx: idx,
        model: crate::model::disk::DiskModel::Virtio,
        media: None,
        boot: false,
        size_mib: Some(k::NOCLOUD_DISK_SIZE_MIB),
        image_uuid: None,
        image_size_mib: None,
        compression: None,
        block_size: Some(u64::from(k::NOCLOUD_BLOCK_SIZE)),
        refreservation: crate::model::disk::Refreservation::Auto,
        pci_slot: None,
        nocreate: false,
        cloudinit_seed: true,
    }
}

/// Assign stable bhyve PCI slots (`assignBhyvePCIslots`): the boot disk on
/// device 4, cdroms on device 3, other data disks on device 5 (matching
/// the slots the bhyve brand boot hook expects). Disks that already carry
/// a `pci_slot` (operator-pinned) are left as-is.
fn assign_pci_slots(disks: &mut [DiskSpec]) {
    let (mut cdrom_fn, mut data_fn) = (0u32, 0u32);
    for d in disks.iter_mut() {
        if d.pci_slot.is_some() {
            continue;
        }
        d.pci_slot = Some(if d.boot {
            "0:4:0".to_string()
        } else if d.effective_media() == DiskMedia::Cdrom {
            let s = format!("0:3:{cdrom_fn}");
            cdrom_fn += 1;
            s
        } else {
            let s = format!("0:5:{data_fn}");
            data_fn += 1;
            s
        });
    }
}

fn normalize_disk(
    zpool: &str,
    vm_uuid: Uuid,
    brand: Brand,
    idx: u32,
    d: DiskPayload,
) -> Result<DiskSpec, VmadmError> {
    let model = match d.model.as_deref() {
        Some(m) => {
            let parsed = DiskModel::parse(m)?;
            // proptable.js model_required=true for both HVM brands, but the
            // valid set differs (bhyve virtio/ahci/nvme vs kvm virtio/scsi/
            // ide). Reject a model that is valid for the other HVM brand.
            if !brand.features().disk_models.contains(&parsed.as_str()) {
                return Err(VmadmError::Payload(format!(
                    "disk model {m:?} is not valid for brand {} (valid: {})",
                    brand.as_str(),
                    brand.features().disk_models.join(",")
                )));
            }
            parsed
        }
        None => return Err(VmadmError::DiskModelRequired),
    };
    let media = match d.media.as_deref() {
        Some("cdrom") => Some(DiskMedia::Cdrom),
        Some("disk") => Some(DiskMedia::Disk),
        Some(other) => {
            return Err(VmadmError::Payload(format!(
                "unsupported disk media {other:?}"
            )));
        }
        None => None,
    };
    let zfs_filesystem = crate::model::disk::disk_dataset(zpool, vm_uuid, idx, brand);
    let raw_path = format!("/dev/zvol/rdsk/{zfs_filesystem}");
    // refreservation arrives in MiB; reject an overflow rather than wrap to
    // a tiny byte count that would silently under-reserve the zvol.
    let refreservation = match d.refreservation {
        Some(0) => Refreservation::None,
        Some(n) => {
            let bytes = n.checked_mul(1024 * 1024).ok_or_else(|| {
                VmadmError::Payload(format!("disk refreservation {n} MiB overflows"))
            })?;
            Refreservation::Bytes(bytes)
        }
        None => Refreservation::Auto,
    };
    Ok(DiskSpec {
        uuid: d.uuid.unwrap_or_else(Uuid::new_v4),
        zfs_filesystem,
        raw_path,
        disk_idx: idx,
        model,
        media,
        boot: d.boot.unwrap_or(false),
        size_mib: d.size,
        image_uuid: d.image_uuid,
        image_size_mib: d.image_size,
        compression: d.compression,
        block_size: d.block_size,
        refreservation,
        pci_slot: d.pci_slot,
        nocreate: d.nocreate.unwrap_or(false),
        cloudinit_seed: false,
    })
}

/// Convert a dotted IPv4 netmask (`255.255.255.0`) to a prefix length,
/// rejecting non-contiguous masks (node validates with `isIPv4Netmask`;
/// a bare `count_ones` would turn `255.0.255.0` into `/16`).
fn netmask_to_prefix(netmask: &str) -> Result<u8, VmadmError> {
    let addr: Ipv4Addr = netmask
        .parse()
        .map_err(|e| VmadmError::Payload(format!("bad netmask {netmask}: {e}")))?;
    let bits = u32::from(addr);
    // A valid mask is a run of 1s followed by a run of 0s: !bits + 1 is a
    // power of two (or bits is all-ones).
    let host = !bits;
    if host & host.wrapping_add(1) != 0 {
        return Err(VmadmError::Payload(format!(
            "netmask {netmask} is not contiguous"
        )));
    }
    Ok(bits.count_ones() as u8)
}

/// Reject characters that would let a payload value break out of the
/// quoted `zonecfg` command stream (CTF-grade defense: hostname, dns
/// domain, resolvers, bhyve-extra-opts all flow into `set value="..."`).
fn reject_zonecfg_text(field: &str, value: &str) -> Result<(), VmadmError> {
    if value.contains(['"', ';', '\n', '\r']) {
        return Err(VmadmError::Payload(format!(
            "{field} contains a character not permitted in zonecfg: {value:?}"
        )));
    }
    Ok(())
}

/// Reject anything outside a conservative identifier set for fields that
/// become zonecfg tokens or zfs dataset components (zpool, nic_tag, model,
/// interface) — these must never carry whitespace or shell/zonecfg metas.
fn reject_zonecfg_ident(field: &str, value: &str) -> Result<(), VmadmError> {
    let ok = !value.is_empty()
        && value
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-' | ':'));
    if !ok {
        return Err(VmadmError::Payload(format!(
            "{field} {value:?} is not a valid identifier"
        )));
    }
    Ok(())
}

/// L8: a create `nic_tag` must match node's `^[a-zA-Z0-9_/]+$` (VM.js:5557).
/// '/' is allowed for dataplane (`proteus[N]`) link names; '.'/'-'/':' are
/// rejected even though the generic ident validator would permit them.
fn reject_nic_tag(tag: &str) -> Result<(), VmadmError> {
    let ok = !tag.is_empty()
        && tag
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '/'));
    if !ok {
        return Err(VmadmError::Payload(format!(
            "nic_tag {tag:?} is not valid (^[a-zA-Z0-9_/]+$)"
        )));
    }
    Ok(())
}

/// L5: the zfs `compression` algorithms node accepts on a disk.
fn reject_compression(value: &str) -> Result<(), VmadmError> {
    const ALLOWED: &[&str] = &[
        "on", "off", "gzip", "gzip-1", "gzip-2", "gzip-3", "gzip-4", "gzip-5", "gzip-6", "gzip-7",
        "gzip-8", "gzip-9", "lz4", "lzjb", "zle",
    ];
    if !ALLOWED.contains(&value) {
        return Err(VmadmError::Payload(format!(
            "disk compression {value:?} is not a supported zfs algorithm"
        )));
    }
    Ok(())
}

/// Validate every free-form spec field that reaches the zonecfg blob or a
/// zfs dataset name. Typed fields (uuids, ips, numbers, the brand/disk
/// model enums) are already safe; `alias` is base64-encoded.
fn validate_spec(spec: &VmCreateSpec) -> Result<(), VmadmError> {
    reject_zonecfg_ident("zpool", &spec.zpool)?;
    // L7: max-shm-memory is emitted to zonecfg in bytes (MiB * 1 MiB).
    // Reject a value that would overflow u64 rather than wrap to a tiny
    // limit when build.rs does the multiply.
    if spec
        .max_shm_memory_mib
        .checked_mul(1024 * 1024)
        .is_none()
    {
        return Err(VmadmError::Payload(format!(
            "max_shm_memory {} MiB overflows when converted to bytes",
            spec.max_shm_memory_mib
        )));
    }
    if let Some(hostname) = &spec.hostname {
        reject_zonecfg_text("hostname", hostname)?;
    }
    reject_zonecfg_text("dns_domain", &spec.dns_domain)?;
    // L9: each resolver must parse as an IpAddr (node validates the same way
    // it validates gateways); reject otherwise rather than emit garbage.
    for r in &spec.resolvers {
        reject_zonecfg_text("resolver", r)?;
        if r.parse::<IpAddr>().is_err() {
            return Err(VmadmError::Payload(format!(
                "resolver {r:?} is not a valid IP address"
            )));
        }
    }
    // M5: alias reaches the NoCloud meta-data seed as well as the (base64)
    // zonecfg attr; block characters that could break out of either stream.
    if let Some(alias) = &spec.alias {
        reject_zonecfg_text("alias", alias)?;
    }
    if let Some(opts) = &spec.bhyve_extra_opts {
        reject_zonecfg_text("bhyve_extra_opts", opts)?;
    }
    // bootrom is a payload-controlled attr value that reaches the quoted
    // zonecfg stream (real values are `uefi`/`bios` or a rom path, so allow
    // `/` but reject the zonecfg metacharacters).
    if let Some(bootrom) = &spec.bootrom {
        reject_zonecfg_text("bootrom", bootrom)?;
    }
    // boot order reaches the quoted HVM attr stream.
    if let Some(boot) = &spec.boot {
        reject_zonecfg_text("boot", boot)?;
    }
    // kernel_version reaches the quoted `kernel-version` attr stream (lx).
    if let Some(kv) = &spec.kernel_version {
        reject_zonecfg_text("kernel_version", kv)?;
    }
    for lp in &spec.limit_priv {
        reject_zonecfg_ident("limit_priv", lp)?;
    }
    for nic in &spec.nics {
        reject_zonecfg_ident("interface", &nic.interface)?;
        // L8: create requires a nic_tag matching node's create regex
        // ^[a-zA-Z0-9_/]+$ (VM.js:5557) — allows '/' for dataplane links,
        // disallows the '.'/'-'/':'  the generic ident validator permits.
        if let Some(tag) = &nic.nic_tag {
            reject_nic_tag(tag)?;
        }
        if let Some(model) = &nic.model {
            reject_zonecfg_ident("nic model", model)?;
        }
    }
    // Disk fields that reach the zonecfg device block (pci-slot is
    // payload-supplied; model is a parsed enum so it is already safe).
    let quota_bytes = spec.quota_gib.saturating_mul(1024 * 1024 * 1024);
    for disk in &spec.disks {
        if let Some(slot) = &disk.pci_slot {
            reject_zonecfg_ident("pci_slot", slot)?;
        }
        // L5: validate the zfs compression algorithm so an unknown value is
        // rejected here rather than failing later at `zfs create`.
        if let Some(comp) = &disk.compression {
            reject_compression(comp)?;
        }
        // H3: a refreservation larger than the zone quota can never be
        // satisfied; reject at normalize instead of failing at zfs.
        if let crate::model::disk::Refreservation::Bytes(b) = disk.refreservation
            && b > quota_bytes
        {
            return Err(VmadmError::Payload(format!(
                "disk refreservation {b} bytes exceeds quota {quota_bytes} bytes"
            )));
        }
    }
    // indestructible_* are accepted by node to refuse destroying a
    // protected dataset, but this engine does not yet place the `zfs hold`
    // that backs them; fail closed rather than silently dropping the
    // protection an operator may rely on.
    if spec.indestructible_zoneroot || spec.indestructible_delegated {
        return Err(VmadmError::Payload(
            "indestructible_zoneroot/indestructible_delegated are not yet supported".to_string(),
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::payload::compat::VmadmCreatePayload;

    fn epoch() -> chrono::DateTime<chrono::Utc> {
        chrono::DateTime::from_timestamp(0, 0).unwrap()
    }

    fn parse(json: serde_json::Value) -> VmCreateSpec {
        let p = VmadmCreatePayload::from_json_slice(json.to_string().as_bytes()).unwrap();
        normalize(p, epoch()).unwrap()
    }

    #[test]
    fn bhyve_memory_overhead_applied() {
        let spec = parse(serde_json::json!({
            "uuid": "11111111-1111-1111-1111-111111111111",
            "brand": "bhyve", "ram": 2048, "vcpus": 2
        }));
        assert_eq!(spec.ram_mib, 2048);
        assert_eq!(spec.max_physical_memory_mib, 2048 + 1280);
        assert_eq!(spec.max_locked_memory_mib, 2048 + 1280);
        assert_eq!(spec.max_swap_mib, 2048 + 1280);
        assert_eq!(
            spec.zfs_filesystem,
            "zones/11111111-1111-1111-1111-111111111111"
        );
        assert_eq!(spec.zonepath, "/zones/11111111-1111-1111-1111-111111111111");
    }

    #[test]
    fn defaults_quota_per_brand() {
        let bhyve = parse(serde_json::json!({"brand":"bhyve","ram":512}));
        assert_eq!(bhyve.quota_gib, 1);
        let jm = parse(serde_json::json!({"brand":"joyent-minimal"}));
        assert_eq!(jm.quota_gib, 10);
        assert_eq!(jm.limit_priv, vec!["default".to_string()]);
        assert_eq!(jm.dns_domain, "local");
    }

    #[test]
    fn kvm_zvols_are_flat_siblings_with_kvm_overhead() {
        // KVM: zvols are siblings zones/<uuid>-diskN (dash), memory overhead
        // is 1024 (not bhyve's 1280), and the zoneroot quota default is the
        // plain 10 GiB (not the bhyve 1 GiB refquota model).
        let spec = parse(serde_json::json!({
            "uuid": "11111111-1111-1111-1111-111111111111",
            "brand": "kvm", "ram": 512, "vcpus": 1,
            "disks": [{ "boot": true, "model": "virtio",
                "image_uuid": "6dde8b1a-0780-11e4-a57b-6b7ac24a7cd4",
                "size": 5120 }]
        }));
        assert_eq!(spec.brand, Brand::Kvm);
        assert_eq!(spec.max_physical_memory_mib, 512 + 1024);
        assert_eq!(spec.max_locked_memory_mib, 512 + 1024);
        assert_eq!(spec.quota_gib, 10);
        // The zoneroot itself stays the plain zones/<uuid> (no -disk suffix).
        assert_eq!(
            spec.zfs_filesystem,
            "zones/11111111-1111-1111-1111-111111111111"
        );
        // The disk is a FLAT SIBLING with a dash, not a child of the root.
        assert_eq!(spec.disks.len(), 1);
        assert_eq!(
            spec.disks[0].zfs_filesystem,
            "zones/11111111-1111-1111-1111-111111111111-disk0"
        );
        assert_eq!(
            spec.disks[0].raw_path,
            "/dev/zvol/rdsk/zones/11111111-1111-1111-1111-111111111111-disk0"
        );
        // The image stays on the disk; no top-level image_uuid was set.
        assert_eq!(spec.image_uuid, None);
        assert_eq!(
            spec.disks[0].image_uuid.map(|u| u.to_string()).as_deref(),
            Some("6dde8b1a-0780-11e4-a57b-6b7ac24a7cd4")
        );
    }

    #[test]
    fn kvm_rejects_bhyve_only_disk_model() {
        // nvme is valid for bhyve but NOT kvm (kvm: virtio/scsi/ide).
        let p = VmadmCreatePayload::from_json_slice(
            serde_json::json!({
                "brand": "kvm", "ram": 512,
                "disks": [{ "boot": true, "model": "nvme", "size": 1024 }]
            })
            .to_string()
            .as_bytes(),
        )
        .unwrap();
        assert!(normalize(p, epoch()).is_err());
        // scsi is kvm-valid but NOT bhyve-valid.
        let p = VmadmCreatePayload::from_json_slice(
            serde_json::json!({
                "brand": "bhyve", "ram": 512,
                "disks": [{ "boot": true, "model": "scsi", "size": 1024 }]
            })
            .to_string()
            .as_bytes(),
        )
        .unwrap();
        assert!(normalize(p, epoch()).is_err());
    }

    #[test]
    fn kvm_top_level_image_uuid_stays_for_spice_root() {
        // Unlike bhyve (which pushes a top-level image onto disk0), kvm keeps
        // a top-level image_uuid for the SPICE-root install (-t/-x nodataset).
        let spec = parse(serde_json::json!({
            "uuid": "11111111-1111-1111-1111-111111111111",
            "brand": "kvm", "ram": 512,
            "image_uuid": "6dde8b1a-0780-11e4-a57b-6b7ac24a7cd4",
            "disks": [{ "boot": true, "model": "virtio", "size": 5120 }]
        }));
        assert_eq!(
            spec.image_uuid.map(|u| u.to_string()).as_deref(),
            Some("6dde8b1a-0780-11e4-a57b-6b7ac24a7cd4")
        );
        // The disk's own image_uuid was NOT populated from the top level.
        assert_eq!(spec.disks[0].image_uuid, None);
    }

    #[test]
    fn lx_is_an_os_zone_with_kernel_version() {
        // lx normalizes like joyent-minimal (OS quota, default limit_priv,
        // tmpfs sized to physical mem, no disks/flex) plus the kernel_version
        // threaded onto the spec.
        let spec = parse(serde_json::json!({
            "uuid": "00000000-0000-0000-0000-000000000001",
            "brand": "lx",
            "image_uuid": "4d3ed29a-c851-11e6-b5a9-639aada4a9c8",
            "kernel_version": "4.4.0",
            "max_physical_memory": 256
        }));
        assert_eq!(spec.brand, Brand::Lx);
        assert_eq!(spec.kernel_version.as_deref(), Some("4.4.0"));
        assert_eq!(spec.quota_gib, 10);
        assert_eq!(spec.limit_priv, vec!["default".to_string()]);
        assert_eq!(spec.tmpfs_mib, Some(256));
        assert!(spec.disks.is_empty());
        assert_eq!(spec.flexible_disk_size_mib, None);
        // The zoneroot image stays at the top level (OS install template),
        // never pushed onto a disk as bhyve does.
        assert_eq!(
            spec.image_uuid.map(|u| u.to_string()).as_deref(),
            Some("4d3ed29a-c851-11e6-b5a9-639aada4a9c8")
        );
    }

    #[test]
    fn lx_without_kernel_version_is_rejected() {
        let p = VmadmCreatePayload::from_json_slice(
            serde_json::json!({
                "brand": "lx",
                "image_uuid": "4d3ed29a-c851-11e6-b5a9-639aada4a9c8"
            })
            .to_string()
            .as_bytes(),
        )
        .unwrap();
        assert!(normalize(p, epoch()).is_err());
    }

    #[test]
    fn non_lx_drops_kernel_version() {
        let spec = parse(serde_json::json!({
            "brand": "joyent-minimal",
            "image_uuid": "4d3ed29a-c851-11e6-b5a9-639aada4a9c8",
            "kernel_version": "4.4.0"
        }));
        assert_eq!(spec.kernel_version, None);
    }

    #[test]
    fn first_nic_becomes_primary() {
        let spec = parse(serde_json::json!({
            "brand": "joyent-minimal",
            "nics": [
                { "nic_tag": "admin", "ips": ["10.0.0.5/24"], "gateways": ["10.0.0.1"] },
                { "nic_tag": "external", "ips": ["192.168.0.5/24"] }
            ]
        }));
        assert!(spec.nics[0].primary);
        assert!(!spec.nics[1].primary);
        assert_eq!(spec.nics[0].interface, "net0");
        assert_eq!(spec.nics[1].interface, "net1");
    }

    #[test]
    fn legacy_ip_netmask_upgraded_to_modern() {
        let spec = parse(serde_json::json!({
            "brand": "joyent-minimal",
            "nics": [{ "nic_tag": "admin", "ip": "10.0.0.5", "netmask": "255.255.255.0",
                       "gateway": "10.0.0.1", "mac": "02:00:00:de:ad:01" }]
        }));
        // node nic.js upgradeNic converts legacy -> ips/gateways.
        assert_eq!(
            spec.nics[0].ips,
            vec![NicAddr::Net("10.0.0.5/24".parse().unwrap())]
        );
        assert_eq!(
            spec.nics[0].gateways,
            vec!["10.0.0.1".parse::<IpAddr>().unwrap()]
        );
    }

    #[test]
    fn bhyve_disk_normalized_with_paths() {
        let spec = parse(serde_json::json!({
            "uuid": "11111111-1111-1111-1111-111111111111",
            "brand": "bhyve", "ram": 1024, "vcpus": 1,
            "disks": [{ "boot": true, "model": "virtio",
                        "image_uuid": "22222222-2222-2222-2222-222222222222",
                        "size": 10240 }]
        }));
        let d = &spec.disks[0];
        assert!(d.boot);
        assert_eq!(d.model, DiskModel::Virtio);
        assert_eq!(
            d.zfs_filesystem,
            "zones/11111111-1111-1111-1111-111111111111/disk0"
        );
        assert_eq!(
            d.raw_path,
            "/dev/zvol/rdsk/zones/11111111-1111-1111-1111-111111111111/disk0"
        );
        assert_eq!(d.refreservation, Refreservation::Auto);
    }

    #[test]
    fn bhyve_disk_requires_model() {
        let p = VmadmCreatePayload::from_json_slice(
            serde_json::json!({
                "brand": "bhyve", "ram": 512,
                "disks": [{ "boot": true, "size": 1024 }]
            })
            .to_string()
            .as_bytes(),
        )
        .unwrap();
        assert!(matches!(
            normalize(p, epoch()),
            Err(VmadmError::DiskModelRequired)
        ));
    }

    fn try_parse(json: serde_json::Value) -> Result<VmCreateSpec, VmadmError> {
        let p = VmadmCreatePayload::from_json_slice(json.to_string().as_bytes()).unwrap();
        normalize(p, epoch())
    }

    #[test]
    fn rejects_zonecfg_injection_in_hostname() {
        // A hostname that tries to break out of the quoted zonecfg value.
        let err = try_parse(serde_json::json!({
            "brand": "joyent-minimal",
            "hostname": "evil\"; create -b; set zonepath=/pwned"
        }))
        .unwrap_err();
        assert!(matches!(err, VmadmError::Payload(_)));
    }

    #[test]
    fn rejects_unsafe_nic_tag_and_zpool() {
        assert!(
            try_parse(serde_json::json!({
                "brand": "joyent-minimal",
                "nics": [{ "nic_tag": "ad min", "ips": ["10.0.0.5/24"] }]
            }))
            .is_err()
        );
        assert!(
            try_parse(serde_json::json!({
                "brand": "joyent-minimal", "zpool": "zones; rm -rf /"
            }))
            .is_err()
        );
    }

    #[test]
    fn rejects_zonecfg_injection_in_every_validated_field() {
        // Each free-form field that reaches the quoted zonecfg stream must
        // reject the metacharacters that would break out of it.
        let evil = "x\"; create -b; set zonepath=/pwned";
        let cases: &[serde_json::Value] = &[
            serde_json::json!({"brand":"bhyve","ram":512,"bootrom":evil}),
            serde_json::json!({"brand":"joyent-minimal","dns_domain":evil}),
            serde_json::json!({"brand":"joyent-minimal","resolvers":[evil]}),
            serde_json::json!({"brand":"bhyve","ram":512,"bhyve_extra_opts":evil}),
            serde_json::json!({"brand":"joyent-minimal","limit_priv":evil}),
            serde_json::json!({"brand":"bhyve","ram":512,
                "disks":[{"boot":true,"model":"virtio","size":1024,
                          "image_uuid":"22222222-2222-2222-2222-222222222222",
                          "pci_slot":"0:4:0\"); add net (x"}]}),
            serde_json::json!({"brand":"joyent-minimal",
                "nics":[{"nic_tag":"admin","ips":["10.0.0.5/24"],"model":evil}]}),
        ];
        for c in cases {
            assert!(
                matches!(try_parse(c.clone()), Err(VmadmError::Payload(_))),
                "field injection not rejected: {c}"
            );
        }
    }

    #[test]
    fn rejects_indestructible_as_unsupported() {
        assert!(matches!(
            try_parse(serde_json::json!({
                "brand": "joyent-minimal", "indestructible_zoneroot": true
            })),
            Err(VmadmError::Payload(_))
        ));
    }

    #[test]
    fn rejects_non_contiguous_netmask() {
        let err = try_parse(serde_json::json!({
            "brand": "joyent-minimal",
            "nics": [{ "nic_tag": "admin", "ip": "10.0.0.5",
                       "netmask": "255.0.255.0", "mac": "02:00:00:de:ad:01" }]
        }))
        .unwrap_err();
        assert!(matches!(err, VmadmError::Payload(_)));
    }

    #[test]
    fn tmpfs_defaults_to_max_physical_for_os_brand() {
        // No tmpfs in payload -> defaults to max_physical_memory.
        let spec = parse(serde_json::json!({
            "brand": "joyent-minimal", "max_physical_memory": 1024
        }));
        assert_eq!(spec.max_physical_memory_mib, 1024);
        assert_eq!(spec.tmpfs_mib, Some(1024));
    }

    #[test]
    fn tmpfs_clamped_to_max_physical() {
        // tmpfs > max_physical_memory is clamped down to it.
        let spec = parse(serde_json::json!({
            "brand": "joyent-minimal", "max_physical_memory": 512, "tmpfs": 4096
        }));
        assert_eq!(spec.tmpfs_mib, Some(512));
    }

    #[test]
    fn tmpfs_respected_when_under_max_physical() {
        let spec = parse(serde_json::json!({
            "brand": "joyent-minimal", "max_physical_memory": 1024, "tmpfs": 256
        }));
        assert_eq!(spec.tmpfs_mib, Some(256));
    }

    #[test]
    fn bhyve_never_gets_default_tmpfs() {
        let spec = parse(serde_json::json!({"brand": "bhyve", "ram": 512}));
        assert_eq!(spec.tmpfs_mib, None);
    }

    #[test]
    fn detects_nocloud_optin() {
        let spec = parse(serde_json::json!({
            "brand": "bhyve", "ram": 512,
            "internal_metadata": { "cloudinit_datasource": "nocloud" }
        }));
        assert!(spec.nocloud);
    }

    #[test]
    fn bhyve_default_limit_priv_locks_down() {
        // H1: an omitted limit_priv stamps the brand lockdown list.
        let spec = parse(serde_json::json!({"brand": "bhyve", "ram": 512}));
        assert_eq!(
            spec.limit_priv,
            vec![
                "default".to_string(),
                "-file_link_any".to_string(),
                "-net_access".to_string(),
                "-proc_fork".to_string(),
                "-proc_info".to_string(),
                "-proc_session".to_string(),
            ]
        );
    }

    #[test]
    fn explicit_limit_priv_overrides_brand_default() {
        let spec = parse(serde_json::json!({
            "brand": "bhyve", "ram": 512, "limit_priv": "default,-net_access"
        }));
        assert_eq!(
            spec.limit_priv,
            vec!["default".to_string(), "-net_access".to_string()]
        );
    }

    #[test]
    fn bhyve_honors_explicit_max_physical_memory() {
        // M3: an explicit max_physical_memory is used verbatim (not
        // ram+overhead) and locked/swap cascade off it.
        let spec = parse(serde_json::json!({
            "brand": "bhyve", "ram": 2048, "max_physical_memory": 4096
        }));
        assert_eq!(spec.max_physical_memory_mib, 4096);
        assert_eq!(spec.max_locked_memory_mib, 4096);
        assert_eq!(spec.max_swap_mib, 4096);
    }

    #[test]
    fn auto_swap_bumped_to_minimum() {
        // L6: with tiny physical the auto swap floors at MINIMUM_MAX_SWAP.
        let spec = parse(serde_json::json!({
            "brand": "joyent-minimal", "max_physical_memory": 64
        }));
        assert_eq!(spec.max_swap_mib, k::MINIMUM_MAX_SWAP_MIB);
    }

    #[test]
    fn explicit_swap_honored_below_minimum() {
        // An explicit max_swap is honored as-is (node only floors the auto
        // path); we must not clamp it up.
        let spec = parse(serde_json::json!({
            "brand": "joyent-minimal", "max_physical_memory": 1024, "max_swap": 128
        }));
        assert_eq!(spec.max_swap_mib, 128);
    }

    #[test]
    fn refreservation_overflow_rejected() {
        // L7: a refreservation in MiB that overflows when scaled to bytes.
        let err = try_parse(serde_json::json!({
            "brand": "bhyve", "ram": 512,
            "disks": [{ "boot": true, "model": "virtio", "size": 1024,
                "image_uuid": "22222222-2222-2222-2222-222222222222",
                "refreservation": u64::MAX }]
        }))
        .unwrap_err();
        assert!(matches!(err, VmadmError::Payload(_)));
    }

    #[test]
    fn refreservation_exceeding_quota_rejected() {
        // H3: refreservation larger than the zone quota can never hold.
        let err = try_parse(serde_json::json!({
            "brand": "bhyve", "ram": 512, "quota": 1,
            "disks": [{ "boot": true, "model": "virtio", "size": 10240,
                "image_uuid": "22222222-2222-2222-2222-222222222222",
                "refreservation": 10240 }]
        }))
        .unwrap_err();
        assert!(matches!(err, VmadmError::Payload(_)));
    }

    #[test]
    fn valid_compression_accepted_unknown_rejected() {
        // L5
        assert!(
            try_parse(serde_json::json!({
                "brand": "bhyve", "ram": 512,
                "disks": [{ "boot": true, "model": "virtio", "size": 1024,
                    "image_uuid": "22222222-2222-2222-2222-222222222222",
                    "compression": "gzip-9" }]
            }))
            .is_ok()
        );
        assert!(
            try_parse(serde_json::json!({
                "brand": "bhyve", "ram": 512,
                "disks": [{ "boot": true, "model": "virtio", "size": 1024,
                    "image_uuid": "22222222-2222-2222-2222-222222222222",
                    "compression": "zstd-fast-1000" }]
            }))
            .is_err()
        );
    }

    #[test]
    fn nic_tag_allows_slash_rejects_dot_dash_colon() {
        // L8: dataplane proteus[N] and admin/external tags are valid; the
        // ':'/'.'/'-' the generic ident permits are not valid for create.
        assert!(
            try_parse(serde_json::json!({
                "brand": "joyent-minimal",
                "nics": [{ "nic_tag": "sdc_underlay/4", "ips": ["10.0.0.5/24"] }]
            }))
            .is_ok()
        );
        for bad in ["a.b", "a-b", "a:b"] {
            assert!(
                try_parse(serde_json::json!({
                    "brand": "joyent-minimal",
                    "nics": [{ "nic_tag": bad, "ips": ["10.0.0.5/24"] }]
                }))
                .is_err(),
                "nic_tag {bad} should be rejected"
            );
        }
    }

    #[test]
    fn resolver_must_be_ip() {
        // L9
        let err = try_parse(serde_json::json!({
            "brand": "joyent-minimal", "resolvers": ["not-an-ip"]
        }))
        .unwrap_err();
        assert!(matches!(err, VmadmError::Payload(_)));
        assert!(
            try_parse(serde_json::json!({
                "brand": "joyent-minimal", "resolvers": ["8.8.8.8", "2001:4860:4860::8888"]
            }))
            .is_ok()
        );
    }

    #[test]
    fn rejects_archive_on_delete_and_imd_namespaces() {
        // L10/L11
        assert!(matches!(
            try_parse(serde_json::json!({
                "brand": "joyent-minimal", "archive_on_delete": true
            })),
            Err(VmadmError::Payload(_))
        ));
        assert!(matches!(
            try_parse(serde_json::json!({
                "brand": "joyent-minimal", "internal_metadata_namespaces": ["ns"]
            })),
            Err(VmadmError::Payload(_))
        ));
    }

    #[test]
    fn vnc_port_and_boot_carried_for_bhyve_only() {
        // M4
        let bhyve = parse(serde_json::json!({
            "brand": "bhyve", "ram": 512, "vnc_port": 5900, "boot": "order=cd"
        }));
        assert_eq!(bhyve.vnc_port, Some(5900));
        assert_eq!(bhyve.boot.as_deref(), Some("order=cd"));
        // OS zones never carry HVM attrs even if the payload sets them.
        let jm = parse(serde_json::json!({
            "brand": "joyent-minimal", "vnc_port": 5900, "boot": "order=cd"
        }));
        assert_eq!(jm.vnc_port, None);
        assert_eq!(jm.boot, None);
    }

    #[test]
    fn bhyve_top_level_image_uuid_pushed_to_boot_disk() {
        // A top-level image_uuid on a bhyve payload must land on disk0, not
        // become the zoneroot install template (which fails with "'devices'
        // does not apply to datasets of this type").
        let spec = parse(serde_json::json!({
            "uuid": "11111111-1111-1111-1111-111111111111",
            "brand": "bhyve", "ram": 1024,
            "image_uuid": "22222222-2222-2222-2222-222222222222",
            "disks": [{ "boot": true, "model": "virtio", "size": 10240 }]
        }));
        // Not passed as the OS-zone install template.
        assert_eq!(spec.image_uuid, None);
        // Applied to the boot disk instead.
        let boot = spec.disks.iter().find(|d| d.boot).expect("boot disk");
        assert_eq!(
            boot.image_uuid,
            Some("22222222-2222-2222-2222-222222222222".parse().unwrap())
        );
    }

    #[test]
    fn bhyve_disk_image_uuid_not_overridden_by_top_level() {
        // A boot disk that already names its own image keeps it; the
        // top-level image_uuid is still dropped from the install template.
        let spec = parse(serde_json::json!({
            "uuid": "11111111-1111-1111-1111-111111111111",
            "brand": "bhyve", "ram": 1024,
            "image_uuid": "22222222-2222-2222-2222-222222222222",
            "disks": [{ "boot": true, "model": "virtio", "size": 10240,
                        "image_uuid": "33333333-3333-3333-3333-333333333333" }]
        }));
        assert_eq!(spec.image_uuid, None);
        let boot = spec.disks.iter().find(|d| d.boot).expect("boot disk");
        assert_eq!(
            boot.image_uuid,
            Some("33333333-3333-3333-3333-333333333333".parse().unwrap())
        );
    }

    #[test]
    fn os_zone_top_level_image_uuid_kept_as_template() {
        // For joyent-minimal the top-level image_uuid IS the zoneroot
        // install template and must be preserved.
        let spec = parse(serde_json::json!({
            "brand": "joyent-minimal",
            "image_uuid": "22222222-2222-2222-2222-222222222222"
        }));
        assert_eq!(
            spec.image_uuid,
            Some("22222222-2222-2222-2222-222222222222".parse().unwrap())
        );
    }

    #[test]
    fn rejects_alias_injection() {
        // M5: alias reaches the NoCloud seed; block metacharacters.
        let err = try_parse(serde_json::json!({
            "brand": "bhyve", "ram": 512,
            "alias": "x\"\nruncmd: [touch, /pwned]"
        }))
        .unwrap_err();
        assert!(matches!(err, VmadmError::Payload(_)));
    }
}
