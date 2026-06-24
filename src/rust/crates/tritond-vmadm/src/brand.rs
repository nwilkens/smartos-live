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
    /// qemu/kvm HVM for IaaS guests. The original SmartOS HVM brand; like
    /// bhyve it has vcpus/zvol disks/vnc, but its zvols are FLAT SIBLINGS
    /// of the zoneroot (`zones/<uuid>-diskN`), the brand owns the zoneroot
    /// (no pre-create), the quota is the simple `quota=Ng` form, and the
    /// disk models are virtio/scsi/ide (proptable.js:2433).
    Kvm,
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
            Brand::Kvm => "kvm",
            Brand::Lx => "lx",
        }
    }

    /// Parse the payload `brand` field. Unsupported brands (`joyent`,
    /// `builder`) are a hard error rather than a silent default.
    pub fn parse(s: &str) -> Result<Self, VmadmError> {
        match s {
            "joyent-minimal" => Ok(Brand::JoyentMinimal),
            "bhyve" => Ok(Brand::Bhyve),
            "kvm" => Ok(Brand::Kvm),
            "lx" => Ok(Brand::Lx),
            other => Err(VmadmError::UnsupportedBrand(other.to_string())),
        }
    }

    /// Whether this brand runs a hardware VM (bhyve/kvm). Drives memory
    /// overhead, the disk/device path, and viona NIC attach. lx is an OS
    /// zone (`features.type == 'LX'`, proptable.js:2460), not HVM.
    #[must_use]
    pub fn is_hvm(self) -> bool {
        matches!(self, Brand::Bhyve | Brand::Kvm)
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
                // OS zones have no zvol disks; the disk-model set is unused.
                disk_models: &[],
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
                // proptable.js:2383 bhyve disk_models.
                disk_models: &["virtio", "ahci", "nvme"],
            },
            // proptable.js:2433 brand_options.kvm.features. KVM is HVM like
            // bhyve, but: it owns the zoneroot (var_svc_provisioning=true,
            // use_vm_autoboot=true so createZone emits `set autoboot=false`
            // + the vm-autoboot attr, VM.js:9718/6978); it is a foreign
            // Linux guest so the engine never writes SmartOS netfiles and
            // never svccfgs an mdata:execute timeout into the zoneroot
            // (those feature keys are absent in proptable.js:2434); its
            // memory overhead is the smaller KVM_MEM_OVERHEAD (1024, not
            // bhyve's 1280); and its disk models are virtio/scsi/ide.
            Brand::Kvm => BrandFeatures {
                // proptable.js:2446 kvm.features.var_svc_provisioning.
                var_svc_provisioning: true,
                zoneinit: false,
                // No write_zone_netfiles key in kvm features (foreign Linux
                // guest); the engine must not drop SmartOS hostname.* files.
                write_zone_netfiles: false,
                // proptable.js:2444 kvm.features.use_vm_autoboot = true.
                use_vm_autoboot: true,
                // HVM: never a tmpfs /tmp.
                use_tmpfs: false,
                // kvm features omit update_mdata_exec_timeout: no SmartOS
                // SMF repository to svccfg against in a Linux zoneroot.
                update_mdata_exec_timeout: false,
                default_memory_overhead_mib: crate::constants::KVM_MEM_OVERHEAD_MIB,
                // proptable.js:2436 kvm.features.limit_priv: the same six-
                // element lockdown as bhyve.
                limit_priv: &[
                    "default",
                    "-file_link_any",
                    "-net_access",
                    "-proc_fork",
                    "-proc_info",
                    "-proc_session",
                ],
                // proptable.js:2449 kvm disk_models.
                disk_models: &["virtio", "scsi", "ide"],
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
                // OS zones have no zvol disks; the disk-model set is unused.
                disk_models: &[],
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
    /// Default `limit_priv` when the payload omits one. bhyve/kvm lock the
    /// guest down (proptable.js:2370/2436); OS brands fall back to
    /// `["default"]` (VM.js:4932). A leading `-` strips a privilege from
    /// `default`.
    pub limit_priv: &'static [&'static str],
    /// Valid `disk.model` values for this brand (proptable.js `disk_models`):
    /// bhyve [virtio,ahci,nvme], kvm [virtio,scsi,ide]. Empty for OS zones
    /// (no zvol disks). Create validates each disk model against this set.
    pub disk_models: &'static [&'static str],
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
        assert_eq!(Brand::parse("kvm").unwrap(), Brand::Kvm);
        assert_eq!(Brand::Kvm.as_str(), "kvm");
        assert!(matches!(
            Brand::parse("joyent"),
            Err(VmadmError::UnsupportedBrand(_))
        ));
    }

    #[test]
    fn bhyve_is_hvm_os_zones_are_not() {
        assert!(Brand::Bhyve.is_hvm());
        assert!(Brand::Kvm.is_hvm());
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

    #[test]
    fn kvm_features_are_hvm_with_smaller_overhead() {
        // proptable.js:2433: KVM is HVM (no tmpfs) but, unlike bhyve, the
        // brand owns the zoneroot (use_vm_autoboot + var_svc_provisioning),
        // it never writes SmartOS netfiles/svccfg into the Linux zoneroot,
        // and its memory overhead is the smaller 1024 (not bhyve's 1280).
        let f = Brand::Kvm.features();
        assert_eq!(
            f.default_memory_overhead_mib,
            crate::constants::KVM_MEM_OVERHEAD_MIB
        );
        assert_ne!(
            f.default_memory_overhead_mib,
            Brand::Bhyve.features().default_memory_overhead_mib
        );
        assert!(!f.use_tmpfs);
        assert!(f.use_vm_autoboot);
        assert!(f.var_svc_provisioning);
        assert!(!f.write_zone_netfiles);
        assert!(!f.update_mdata_exec_timeout);
        // Same six-element lockdown as bhyve.
        assert_eq!(f.limit_priv, Brand::Bhyve.features().limit_priv);
        // kvm disk models differ from bhyve.
        assert_eq!(f.disk_models, &["virtio", "scsi", "ide"]);
        assert_eq!(Brand::Bhyve.features().disk_models, &["virtio", "ahci", "nvme"]);
    }
}
