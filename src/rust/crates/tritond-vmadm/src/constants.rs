// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.
//
// Copyright 2026 Edgecast Cloud LLC.

//! Constants lifted verbatim from node `VM.js` / `proptable.js` so the
//! native engine reproduces SmartOS behaviour exactly. Each value is
//! annotated with its source so a future divergence is auditable.

/// bhyve guest memory overhead in MiB (`proptable.js`: `1024 + 256`).
/// `max_physical_memory = ram + BHYVE_MEM_OVERHEAD`.
pub const BHYVE_MEM_OVERHEAD_MIB: u64 = 1024 + 256;

/// KVM guest memory overhead in MiB (`proptable.js`). Retained for the
/// get-JSON `max_physical_memory` reconstruction only; vNext does not
/// provision KVM.
pub const KVM_MEM_OVERHEAD_MIB: u64 = 1024;

/// Default zoneroot quota for an OS zone, GiB (`VM.js` `DEFAULT_QUOTA`).
pub const DEFAULT_QUOTA_GIB: u64 = 10;

/// Default zoneroot refquota for a bhyve zone, GiB
/// (`VM.js` `DEFAULT_BHYVE_QUOTA`).
pub const DEFAULT_BHYVE_QUOTA_GIB: u64 = 1;

/// Provisioning transition timeout in seconds (`VM.js` `PROVISION_TIMEOUT`).
/// For bhyve the timeout is multiplied by `1 + max(0, floor(ram/4096) - 1)`;
/// OS zones use it as-is.
pub const PROVISION_TIMEOUT_SECS: u64 = 300;

/// Graceful-stop timeout in seconds (`VM.js` `STOP_TIMEOUT = 60`). Bounds
/// the in-guest shutdown before a stop falls back to a halt.
pub const STOP_TIMEOUT_SECS: u64 = 60;

/// Default `mdata:execute` SMF start timeout written at create
/// (`VM.js` default).
pub const DEFAULT_MDATA_EXEC_TIMEOUT_SECS: u64 = 300;

/// Default `cpu_shares` (`VM.js` `applyZoneDefaults`).
pub const DEFAULT_CPU_SHARES: u32 = 100;

/// Default `zfs_io_priority` (`VM.js`).
pub const DEFAULT_ZFS_IO_PRIORITY: u32 = 100;

/// Default `max_lwps` (`VM.js`).
pub const DEFAULT_MAX_LWPS: u32 = 2000;

/// Default System-V IPC limits (`VM.js` `applyZoneDefaults`). `max_shm_memory`
/// defaults to `max_physical_memory` instead.
pub const DEFAULT_MAX_MSG_IDS: u32 = 4096;
pub const DEFAULT_MAX_SEM_IDS: u32 = 4096;
pub const DEFAULT_MAX_SHM_IDS: u32 = 4096;

/// Default guest RAM in MiB when unspecified (`VM.js`).
pub const DEFAULT_RAM_MIB: u64 = 256;

/// Minimum `max_swap` in MiB when the engine auto-defaults it
/// (`VM.js:153 MINIMUM_MAX_SWAP`). Only enforced on the auto path; an
/// explicit payload `max_swap` is honored as-is.
pub const MINIMUM_MAX_SWAP_MIB: u64 = 256;

/// Default DNS domain (`VM.js`).
pub const DEFAULT_DNS_DOMAIN: &str = "local";

/// Default zpool (`VM.js`).
pub const DEFAULT_ZPOOL: &str = "zones";

/// The all-zero UUID used for `owner_uuid` / `billing_id` defaults.
pub const ZERO_UUID: uuid::Uuid = uuid::Uuid::nil();

// ----- NoCloud cloud-init seed disk (vNext `cloudinit/lofs-fat16.js`) -----

/// NoCloud seed disk size, MiB.
pub const NOCLOUD_DISK_SIZE_MIB: u64 = 16;

/// NoCloud FAT block size, bytes.
pub const NOCLOUD_BLOCK_SIZE: u32 = 512;

/// NoCloud FAT volume label (`mkfs -F pcfs -o b=CIDATA`).
pub const NOCLOUD_LABEL: &str = "CIDATA";

/// NoCloud FAT sector count: `16 MiB / 512`.
pub const NOCLOUD_SECTORS: u64 = NOCLOUD_DISK_SIZE_MIB * 1024 * 1024 / NOCLOUD_BLOCK_SIZE as u64;

/// Default volblocksize when none is requested and the pool is not a
/// 4K-ashift raidz (`VM.js` `getOptimalVolblocksize` fallback).
pub const DEFAULT_VOLBLOCKSIZE: u64 = 8192;

/// The path under a zoneroot whose replacement by the in-zone mdata SMF
/// signals provisioning completion.
pub const PROVISIONING_MARKER: &str = "root/var/svc/provisioning";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nocloud_sectors_is_32768() {
        assert_eq!(NOCLOUD_SECTORS, 32768);
    }

    #[test]
    fn bhyve_overhead_is_1280() {
        assert_eq!(BHYVE_MEM_OVERHEAD_MIB, 1280);
    }
}
