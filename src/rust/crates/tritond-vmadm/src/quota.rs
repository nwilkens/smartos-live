// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.
//
// Copyright 2026 Edgecast Cloud LLC.

//! The bhyve zoneroot quota math (RFD 0148 snapspace), a pure port of
//! node `VM.js` `setQuotaBhyve` (verified at VM.js:1508). The applier that
//! drives the `zfs set` calls lives in the create path; this module is
//! just the arithmetic so it is exhaustively unit-testable.

use crate::constants as k;

const MIB: u64 = 1024 * 1024;
const GIB: u64 = 1024 * 1024 * 1024;

/// A quota/reservation value: bytes, or the `<N>g` override form node
/// emits when the requested quota exceeds the computed one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QuotaValue {
    Bytes(u64),
    Gigs(u64),
}

impl QuotaValue {
    /// The `zfs set quota=<v>` argument form.
    #[must_use]
    pub fn zfs_arg(self) -> String {
        match self {
            QuotaValue::Bytes(b) => b.to_string(),
            QuotaValue::Gigs(g) => format!("{g}g"),
        }
    }
}

/// The three values to set on the zoneroot dataset after the child zvols'
/// `refreservation=auto` has been read back.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct QuotaPlan {
    pub quota: QuotaValue,
    pub reservation: QuotaValue,
    pub refreservation_bytes: u64,
}

/// Compute the zoneroot `quota`/`reservation`/`refreservation` per
/// RFD 0148 (`setQuotaBhyve`).
///
/// - `quota_gib`: the payload `quota` (min `DEFAULT_BHYVE_QUOTA` applies).
/// - `flexible_disk_size_mib`: the flexible-disk budget, if set.
/// - `volsize_bytes`: Σ child-volume `volsize`.
/// - `volrefres_bytes`: Σ child-volume `refreservation` (after `=auto`).
#[must_use]
pub fn compute_bhyve_quota(
    quota_gib: u64,
    flexible_disk_size_mib: Option<u64>,
    volsize_bytes: u64,
    volrefres_bytes: u64,
) -> QuotaPlan {
    // maxQuota = max(DEFAULT_BHYVE_QUOTA, quota) GiB  (VM.js:1527)
    let max_quota = quota_gib.max(k::DEFAULT_BHYVE_QUOTA_GIB) * GIB;
    // flexsize is in MiB to match disk.*.size units (VM.js:1530).
    let flexsize = flexible_disk_size_mib.unwrap_or(0) * MIB;

    // VM.js:1635 — the flex form subtracts volsize (it is already counted
    // in flexsize); the non-flex form does not.
    let computed = if flexsize != 0 {
        volrefres_bytes
            .saturating_add(max_quota)
            .saturating_add(flexsize)
            .saturating_sub(volsize_bytes)
    } else {
        volrefres_bytes.saturating_add(max_quota)
    };

    // Override: if the requested quota (in bytes) exceeds the computed
    // value, emit it as `<quota_gib>g` (VM.js:1643).
    let value = if quota_gib * GIB > computed {
        QuotaValue::Gigs(quota_gib)
    } else {
        QuotaValue::Bytes(computed)
    };

    QuotaPlan {
        quota: value,
        reservation: value,
        // 1 MiB headroom on the topmost dataset (VM.js:1655).
        refreservation_bytes: max_quota.saturating_sub(MIB),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // A 20 GiB flexible-disk guest with a single 20 GiB boot zvol whose
    // auto refreservation came back at ~20.6 GiB (typical 8k volblocksize
    // overhead).
    #[test]
    fn flex_form_subtracts_volsize() {
        let volsize = 20 * GIB;
        let volrefres = 22_106_232_832; // ~20.6 GiB
        let plan = compute_bhyve_quota(1, Some(20 * 1024), volsize, volrefres);
        // quota = volrefres + 1GiB + (20GiB in MiB)*MiB - volsize
        let want = volrefres + GIB + (20 * 1024 * MIB) - volsize;
        assert_eq!(plan.quota, QuotaValue::Bytes(want));
        assert_eq!(plan.reservation, QuotaValue::Bytes(want));
        assert_eq!(plan.refreservation_bytes, GIB - MIB);
    }

    #[test]
    fn non_flex_form_is_volrefres_plus_maxquota() {
        let volrefres = 11 * GIB;
        let plan = compute_bhyve_quota(10, None, 10 * GIB, volrefres);
        // maxQuota = max(1,10)=10GiB; quota = volrefres + 10GiB
        assert_eq!(plan.quota, QuotaValue::Bytes(volrefres + 10 * GIB));
        assert_eq!(plan.refreservation_bytes, 10 * GIB - MIB);
    }

    #[test]
    fn override_floors_quota_to_requested_gigs() {
        // A pathological flex config where the computed value falls below
        // the requested quota: the override emits "<g>g" as the floor.
        // computed = 5 + 10 + 1 - 50 GiB -> saturates to 0; 10GiB > 0.
        let plan = compute_bhyve_quota(10, Some(1024), 50 * GIB, 5 * GIB);
        assert_eq!(plan.quota, QuotaValue::Gigs(10));
        assert_eq!(plan.reservation, QuotaValue::Gigs(10));
    }

    #[test]
    fn no_override_when_computed_exceeds_requested() {
        // computed = 1GiB + max(1,100)*GiB = 101GiB > requested 100GiB.
        let plan = compute_bhyve_quota(100, None, GIB, GIB);
        assert_eq!(plan.quota, QuotaValue::Bytes(101 * GIB));
    }

    #[test]
    fn zero_disk_guest() {
        // Live-CD style: no volumes.
        let plan = compute_bhyve_quota(1, Some(10 * 1024), 0, 0);
        // quota = 0 + 1GiB + 10GiB - 0
        assert_eq!(plan.quota, QuotaValue::Bytes(GIB + 10 * GIB));
    }

    #[test]
    fn quota_value_zfs_arg() {
        assert_eq!(QuotaValue::Bytes(1024).zfs_arg(), "1024");
        assert_eq!(QuotaValue::Gigs(25).zfs_arg(), "25g");
    }
}
