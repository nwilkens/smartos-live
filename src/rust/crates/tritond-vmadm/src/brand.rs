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
    /// Linux-userland branded zone on the illumos kernel. Installs and
    /// runs through the OS-zone path (zfs clone of an lx-dataset image),
    /// differing from joyent-minimal only in `brand=lx` and the
    /// `kernel-version` attr (proptable.js:2451).
    Lx,
}

impl Brand {
    /// The on-the-wire zonecfg/payload brand string.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Brand::JoyentMinimal => "joyent-minimal",
            Brand::Bhyve => "bhyve",
            Brand::Lx => "lx",
        }
    }

    /// Parse the payload `brand` field. Unsupported brands (`kvm`,
    /// `joyent`, `builder`) are a hard error rather than a silent default.
    pub fn parse(s: &str) -> Result<Self, VmadmError> {
        match s {
            "joyent-minimal" => Ok(Brand::JoyentMinimal),
            "bhyve" => Ok(Brand::Bhyve),
            "lx" => Ok(Brand::Lx),
            other => Err(VmadmError::UnsupportedBrand(other.to_string())),
        }
    }

    /// Whether this brand runs a hardware VM (bhyve). Drives memory
    /// overhead, the disk/device path, and viona NIC attach. lx is an OS
    /// zone (`features.type == 'LX'`, proptable.js:2460), not HVM.
    #[must_use]
    pub fn is_hvm(self) -> bool {
        matches!(self, Brand::Bhyve)
    }

    /// Whether this brand installs and runs through the OS-zone code path
    /// (joyent-minimal and lx). Both clone an image as the zoneroot and
    /// share the install + boot lifecycle; the lx delta is the
    /// `kernel-version` attr (proptable.js:2451 reuses the joyent-minimal
    /// jinstall flow via `zoneadm install -t`).
    #[must_use]
    pub fn is_os_zone(self) -> bool {
        matches!(self, Brand::JoyentMinimal | Brand::Lx)
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
                // proptable.js:2426 joyent-minimal.update_mdata_exec_timeout.
                update_mdata_exec_timeout: true,
                default_memory_overhead_mib: 0,
                // joyent-minimal carries no features.limit_priv, so
                // applyZoneDefaults falls back to ["default"] (VM.js:4932).
                limit_priv: &["default"],
            },
            Brand::Bhyve => BrandFeatures {
                var_svc_provisioning: false,
                zoneinit: false,
                write_zone_netfiles: false,
                use_vm_autoboot: false,
                use_tmpfs: false,
                // bhyve carries update_mdata_exec_timeout; its zoneroot has
                // a SmartOS SMF repository (proptable.js bhyve features).
                update_mdata_exec_timeout: true,
                default_memory_overhead_mib: crate::constants::BHYVE_MEM_OVERHEAD_MIB,
                // proptable.js brand_options.bhyve.features.limit_priv:
                // strips file_link_any/net_access/proc_* so a guest cannot
                // reach the global zone (proptable.js:2370).
                limit_priv: &[
                    "default",
                    "-file_link_any",
                    "-net_access",
                    "-proc_fork",
                    "-proc_info",
                    "-proc_session",
                ],
            },
            // proptable.js:2451 brand_options.lx.features. lx mirrors
            // joyent-minimal's OS sizing/priv model; the only behavioural
            // delta the engine cares about is write_zone_netfiles=false
            // (the lx brand writes the Linux net config itself, so the
            // engine must NOT drop SmartOS /etc/hostname.* files into the
            // zoneroot, proptable.js:2469).
            Brand::Lx => BrandFeatures {
                // VM.js:9553 checkDatasetProvisionable short-circuits to
                // true for features.type == 'LX' (the lx image always
                // exposes /var/svc/provisioning via the brand).
                var_svc_provisioning: true,
                // lx has no joyent zoneinit.
                zoneinit: false,
                // proptable.js:2469 lx.features.write_zone_netfiles = false.
                write_zone_netfiles: false,
                use_vm_autoboot: false,
                // proptable.js:2466 lx.features.use_tmpfs = true; OS-style
                // tmpfs sized to max_physical_memory.
                use_tmpfs: true,
                // lx omits update_mdata_exec_timeout (proptable.js:2451): a
                // Linux userland has no illumos SMF repository, so the
                // create path must NOT run svccfg against the zoneroot.
                update_mdata_exec_timeout: false,
                // OS zone: no memory overhead added to ram.
                default_memory_overhead_mib: 0,
                // lx carries no features.limit_priv, so applyZoneDefaults
                // falls back to ["default"] (VM.js:4932).
                limit_priv: &["default"],
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
    /// Brand exposes the SMF `svc:/smartdc/mdata:execute` service whose
    /// start timeout the create path tunes via `svccfg`. lx has a Linux
    /// userland (no illumos SMF repository in the zoneroot), so its
    /// features omit `update_mdata_exec_timeout` and the engine must skip
    /// the svccfg call (proptable.js: only the OS/HVM SmartOS brands carry
    /// it; VM.js:9103 gates the call on this flag).
    pub update_mdata_exec_timeout: bool,
    /// MiB added to `ram` to derive `max_physical_memory`.
    pub default_memory_overhead_mib: u64,
    /// Default `limit_priv` when the payload omits one. bhyve locks the
    /// guest down (proptable.js:2370); OS brands fall back to `["default"]`
    /// (VM.js:4932). A leading `-` strips a privilege from `default`.
    pub limit_priv: &'static [&'static str],
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
        assert_eq!(Brand::parse("lx").unwrap(), Brand::Lx);
        assert_eq!(Brand::Lx.as_str(), "lx");
        assert!(matches!(
            Brand::parse("kvm"),
            Err(VmadmError::UnsupportedBrand(_))
        ));
        assert!(matches!(
            Brand::parse("joyent"),
            Err(VmadmError::UnsupportedBrand(_))
        ));
    }

    #[test]
    fn bhyve_is_hvm_os_zones_are_not() {
        assert!(Brand::Bhyve.is_hvm());
        assert!(!Brand::JoyentMinimal.is_hvm());
        assert!(!Brand::Lx.is_hvm());
        // lx and joyent-minimal share the OS-zone install/lifecycle path.
        assert!(Brand::Lx.is_os_zone());
        assert!(Brand::JoyentMinimal.is_os_zone());
        assert!(!Brand::Bhyve.is_os_zone());
    }

    #[test]
    fn lx_features_match_os_model_with_no_netfiles() {
        // proptable.js:2451: lx is an OS zone (default limit_priv, tmpfs,
        // no memory overhead) but, unlike joyent-minimal, the brand owns
        // the in-zone net config so the engine must not write netfiles.
        let f = Brand::Lx.features();
        assert_eq!(f.limit_priv, &["default"]);
        assert!(f.use_tmpfs);
        assert!(f.var_svc_provisioning);
        assert_eq!(f.default_memory_overhead_mib, 0);
        assert!(!f.write_zone_netfiles);
    }

    #[test]
    fn bhyve_limit_priv_locks_down_guest() {
        // proptable.js:2370: the six-element lockdown list verbatim.
        assert_eq!(
            Brand::Bhyve.features().limit_priv,
            &[
                "default",
                "-file_link_any",
                "-net_access",
                "-proc_fork",
                "-proc_info",
                "-proc_session",
            ]
        );
        assert_eq!(Brand::JoyentMinimal.features().limit_priv, &["default"]);
    }
}
