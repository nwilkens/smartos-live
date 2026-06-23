// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.
//
// Copyright 2026 Edgecast Cloud LLC.

//! Normalized bhyve disk model. OS (joyent-minimal) zones have no disks
//! here; their storage is the zoneroot dataset created by `zoneadm
//! install`.

use uuid::Uuid;

use crate::error::VmadmError;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiskModel {
    Virtio,
    Ahci,
    Nvme,
}

impl DiskModel {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            DiskModel::Virtio => "virtio",
            DiskModel::Ahci => "ahci",
            DiskModel::Nvme => "nvme",
        }
    }

    pub fn parse(s: &str) -> Result<Self, VmadmError> {
        match s {
            "virtio" => Ok(DiskModel::Virtio),
            "ahci" => Ok(DiskModel::Ahci),
            "nvme" => Ok(DiskModel::Nvme),
            other => Err(VmadmError::Payload(format!(
                "unsupported bhyve disk model {other:?}"
            ))),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiskMedia {
    Disk,
    Cdrom,
}

impl DiskMedia {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            DiskMedia::Disk => "disk",
            DiskMedia::Cdrom => "cdrom",
        }
    }
}

/// How a zvol's `refreservation` is set at create. `Auto` lets ZFS
/// compute it (the snapspace path); the computed value is read back for
/// the quota math.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Refreservation {
    Auto,
    Bytes(u64),
    None,
}

/// A bhyve disk after normalization.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiskSpec {
    pub uuid: Uuid,
    /// `{zpool}/{vm_uuid}/disk{idx}`.
    pub zfs_filesystem: String,
    /// `/dev/zvol/rdsk/{zfs_filesystem}` (raw, for mkfs/clone).
    pub raw_path: String,
    pub disk_idx: u32,
    pub model: DiskModel,
    /// `None` when the payload omitted `media` (node then emits no `media`
    /// property and the brand defaults to a disk). Use [`Self::effective_media`]
    /// for logic that needs a concrete value.
    pub media: Option<DiskMedia>,
    pub boot: bool,
    /// Disk size in MiB. `None` only for image-only disks before the
    /// image size is resolved.
    pub size_mib: Option<u64>,
    pub image_uuid: Option<Uuid>,
    pub image_size_mib: Option<u64>,
    pub compression: Option<String>,
    /// `volblocksize` in bytes.
    pub block_size: Option<u64>,
    pub refreservation: Refreservation,
    /// `bus:dev:fn` PCI slot for the device resource.
    pub pci_slot: Option<String>,
    /// Disk pre-exists (do not `zfs create`/`clone`).
    pub nocreate: bool,
    /// The 16 MiB NoCloud CIDATA seed disk; not persisted to the VM JSON.
    pub cloudinit_seed: bool,
}

impl DiskSpec {
    /// `/dev/zvol/dsk/{zfs_filesystem}` (cooked block device, for the
    /// device `match=` and for opening the seed volume).
    #[must_use]
    pub fn block_path(&self) -> String {
        format!("/dev/zvol/dsk/{}", self.zfs_filesystem)
    }

    /// The concrete media for logic (createVolumes skips cdroms); an
    /// unspecified `media` is a disk.
    #[must_use]
    pub fn effective_media(&self) -> DiskMedia {
        self.media.unwrap_or(DiskMedia::Disk)
    }
}
