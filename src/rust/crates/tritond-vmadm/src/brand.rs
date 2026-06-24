// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.
//
// Copyright 2026 Edgecast Cloud LLC.

//! The two brands this engine supports and the per-brand feature flags
//! the create/lifecycle code branches on. This is the trimmed analogue of
//! node `proptable.js` `BRAND_OPTIONS[brand].features`, carrying only the
//! flags vNext actually uses.

use crate::error::VmadmError;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Brand {
    /// Native illumos zone for control-plane services.
    JoyentMinimal,
    /// bhyve HVM for IaaS guests.
    Bhyve,
}

impl Brand {
    /// The on-the-wire zonecfg/payload brand string.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Brand::JoyentMinimal => "joyent-minimal",
            Brand::Bhyve => "bhyve",
        }
    }

    /// Parse the payload `brand` field. Unsupported brands (`kvm`, `lx`,
    /// `joyent`, `builder`) are a hard error rather than a silent default.
    pub fn parse(s: &str) -> Result<Self, VmadmError> {
        match s {
            "joyent-minimal" => Ok(Brand::JoyentMinimal),
            "bhyve" => Ok(Brand::Bhyve),
            other => Err(VmadmError::UnsupportedBrand(other.to_string())),
        }
    }

    /// Whether this brand runs a hardware VM (bhyve). Drives memory
    /// overhead, the disk/device path, and viona NIC attach.
    #[must_use]
    pub fn is_hvm(self) -> bool {
        matches!(self, Brand::Bhyve)
    }

    /// The brand's features, mirroring the subset of
    /// `proptable.js` `features` the engine consults.
    #[must_use]
    pub fn features(self) -> BrandFeatures {
        match self {
            Brand::JoyentMinimal => BrandFeatures {
                // var_svc_provisioning: the brand always exposes
                // /var/svc/provisioning, so checkDatasetProvisionable is
                // satisfied unconditionally (VM.js:9514).
                var_svc_provisioning: true,
                // joyent-minimal has no zoneinit -> no /root/zoneconfig
                // and cleanupMessyDataset additionally removes zoneinit.*.
                zoneinit: false,
                write_zone_netfiles: true,
                use_vm_autoboot: false,
                // OS brands default tmpfs to max_physical_memory
                // (proptable.js use_tmpfs); HVM brands never get tmpfs.
                use_tmpfs: true,
                default_memory_overhead_mib: 0,
            },
            Brand::Bhyve => BrandFeatures {
                var_svc_provisioning: false,
                zoneinit: false,
                write_zone_netfiles: false,
                use_vm_autoboot: false,
                use_tmpfs: false,
                default_memory_overhead_mib: crate::constants::BHYVE_MEM_OVERHEAD_MIB,
            },
        }
    }
}

/// Per-brand behaviour flags (subset of node `proptable.js` features).
#[derive(Debug, Clone, Copy)]
pub struct BrandFeatures {
    /// Brand always provides `/var/svc/provisioning`; create needs no
    /// dataset inspection to confirm provisionability.
    pub var_svc_provisioning: bool,
    /// Brand runs the legacy zoneinit (writes `/root/zoneconfig`).
    pub zoneinit: bool,
    /// Brand wants `/etc/hostname.<if>` / `defaultrouter` / `nodename`
    /// written into the zoneroot at create.
    pub write_zone_netfiles: bool,
    /// Brand keeps `autoboot` as a brand attr (`set autoboot=false` in the
    /// immutable header). Both supported brands keep it as a zonecfg
    /// scalar instead, so this is `false` for both.
    pub use_vm_autoboot: bool,
    /// Brand mounts a tmpfs `/tmp`; when the payload omits `tmpfs`, node
    /// defaults it to `max_physical_memory` (MiB). HVM brands skip this.
    pub use_tmpfs: bool,
    /// MiB added to `ram` to derive `max_physical_memory`.
    pub default_memory_overhead_mib: u64,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_supported_and_reject_others() {
        assert_eq!(Brand::parse("bhyve").unwrap(), Brand::Bhyve);
        assert_eq!(
            Brand::parse("joyent-minimal").unwrap(),
            Brand::JoyentMinimal
        );
        assert!(matches!(
            Brand::parse("kvm"),
            Err(VmadmError::UnsupportedBrand(_))
        ));
        assert!(matches!(
            Brand::parse("lx"),
            Err(VmadmError::UnsupportedBrand(_))
        ));
    }

    #[test]
    fn bhyve_is_hvm_jm_is_not() {
        assert!(Brand::Bhyve.is_hvm());
        assert!(!Brand::JoyentMinimal.is_hvm());
    }
}
