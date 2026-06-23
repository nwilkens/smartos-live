// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.
//
// Copyright 2026 Edgecast Cloud LLC.

//! Normalized NIC model + the MAC <-> zonecfg form helpers.

use std::net::IpAddr;

use ipnetwork::IpNetwork;

/// A NIC after normalization. `interface` is assigned `net{idx}`;
/// `nic_tag` becomes the zonecfg `global-nic` (the agent already supplies
/// the realized link there; the [`crate::nictag::NicTagRegistrar`]
/// guarantees it is a registered tag before boot).
///
/// Addresses are always in the MODERN form (`ips`/`gateways`): node's
/// `nic.js` `upgradeNic` converts a legacy `ip`+`netmask`/`gateway`
/// payload into `ips=[ip/bits]`/`gateways=[gw]` before zonecfg generation,
/// so the legacy form never reaches the zone config. The `allow_*` flags
/// are `Option` so an absent flag emits nothing (node only emits the ones
/// the payload set).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NicSpec {
    /// Canonical lowercase MAC, `aa:bb:cc:dd:ee:ff`.
    pub mac: String,
    /// `net{idx}`.
    pub interface: String,
    /// SmartOS nic_tag name (e.g. `admin`, `external`, `proteus49377`).
    pub nic_tag: Option<String>,
    /// VLAN id. `Some` (including `Some(0)`) is emitted to zonecfg, matching
    /// node's `vlan_id !== '0'` quirk where a numeric `0` still emits a
    /// `set vlan-id=0`; `None` (the field absent from the payload) is not.
    pub vlan_id: Option<u16>,
    /// CIDR entries plus the `dhcp` / `addrconf` pseudo-values.
    pub ips: Vec<NicAddr>,
    pub gateways: Vec<IpAddr>,
    pub primary: bool,
    pub model: Option<String>,
    pub mtu: Option<u32>,
    pub allow_ip_spoofing: Option<bool>,
    pub allow_mac_spoofing: Option<bool>,
    pub allow_dhcp_spoofing: Option<bool>,
    pub allow_restricted_traffic: Option<bool>,
    pub allow_unfiltered_promisc: Option<bool>,
    pub allowed_ips: Vec<IpNetwork>,
}

/// An entry in a NIC's `ips` list: either a real network or one of the
/// two pseudo-values node-vmadm accepts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NicAddr {
    Net(IpNetwork),
    Dhcp,
    Addrconf,
}

impl NicAddr {
    /// The raw string form node stores in the `ips` zonecfg property
    /// (`10.0.0.5/24`, `dhcp`, `addrconf`).
    #[must_use]
    pub fn as_string(&self) -> String {
        match self {
            NicAddr::Net(n) => n.to_string(),
            NicAddr::Dhcp => "dhcp".to_string(),
            NicAddr::Addrconf => "addrconf".to_string(),
        }
    }

    /// The representative IPv4 of a NIC (the first real v4 net), or `None`
    /// when the NIC is dhcp/addrconf/v6-only. Mirrors
    /// `ipaddr.getRepresentativeIP(ips, 'ipv4')`.
    #[must_use]
    pub fn representative_ipv4(ips: &[NicAddr]) -> Option<IpNetwork> {
        ips.iter().find_map(|a| match a {
            NicAddr::Net(n @ IpNetwork::V4(_)) => Some(*n),
            _ => None,
        })
    }
}

/// Strip leading zeros from each octet of a MAC, the form `zonecfg`
/// requires for `set mac-addr=` (node `VM.js` `ruinMac`): `01:02:0a` ->
/// `1:2:a`. A zero octet collapses to a single `0`.
#[must_use]
pub fn ruin_mac(mac: &str) -> String {
    mac.split(':')
        .map(|octet| {
            let trimmed = octet.trim_start_matches('0');
            if trimmed.is_empty() { "0" } else { trimmed }
        })
        .collect::<Vec<_>>()
        .join(":")
}

/// Re-pad a possibly ruined MAC back to canonical `aa:bb:..` form with
/// two lowercase hex digits per octet (node `VM.js` `fixMac`). Used when
/// reading a MAC back out of zonecfg for `vmadm get`.
#[must_use]
pub fn fix_mac(mac: &str) -> String {
    mac.split(':')
        .map(|octet| format!("{:0>2}", octet.to_lowercase()))
        .collect::<Vec<_>>()
        .join(":")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ruin_mac_strips_leading_zeros() {
        assert_eq!(ruin_mac("02:00:00:de:ad:01"), "2:0:0:de:ad:1");
        assert_eq!(ruin_mac("00:00:00:00:00:00"), "0:0:0:0:0:0");
        assert_eq!(ruin_mac("aa:bb:cc:dd:ee:ff"), "aa:bb:cc:dd:ee:ff");
    }

    #[test]
    fn fix_mac_repads() {
        assert_eq!(fix_mac("2:0:0:de:ad:1"), "02:00:00:de:ad:01");
        assert_eq!(fix_mac("0:0:0:0:0:0"), "00:00:00:00:00:00");
    }

    #[test]
    fn ruin_then_fix_roundtrips_canonical() {
        let m = "02:00:00:de:ad:01";
        assert_eq!(fix_mac(&ruin_mac(m)), m);
    }

    #[test]
    fn representative_ipv4_picks_first_v4() {
        let ips = vec![NicAddr::Dhcp, NicAddr::Net("10.0.0.5/24".parse().unwrap())];
        assert_eq!(
            NicAddr::representative_ipv4(&ips),
            Some("10.0.0.5/24".parse().unwrap())
        );
        assert_eq!(NicAddr::representative_ipv4(&[NicAddr::Dhcp]), None);
    }
}
