// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.
//
// Copyright 2026 Edgecast Cloud LLC.

//! ipf compiler. Faithful port of the rule->ipf.conf generation in
//! `src/fw/lib/fw.js`: prepareIPFdata, ipfRuleObj, protoTarget, compareRules,
//! vmsOnSide, rulesFromOtherSide, noRulesNeeded, and the fixed fallback rules.
//!
//! NOTE: this codebase compiles each IP/subnet/VM-IP target to an INLINE ipf
//! rule with the literal address; there is no ippool/table-pool generation
//! (confirmed: zero ippool references in src/fw/lib).

use std::cmp::Ordering;
use std::collections::BTreeMap;

use crate::model::{FwRule, ProtoTargets, Side, TagEntry};
use crate::parse::PortTarget;
use crate::vm::{Vm, VmLookup};

const KEEP_FRAGS: &str = " keep frags";
const KEEP_STATE: &str = " keep state";

/// Direction of a rule side.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Dir {
    From,
    To,
}

impl Dir {
    fn as_str(self) -> &'static str {
        match self {
            Dir::From => "from",
            Dir::To => "to",
        }
    }
}

/// Generated ipf config for one VM/zone.
#[derive(Debug, Clone)]
pub struct IpfConf {
    /// VM uuid.
    pub uuid: String,
    /// Zonepath.
    pub zonepath: String,
    /// IPv4 ipf.conf body (with trailing newline).
    pub v4text: String,
    /// IPv6 ipf6.conf body (with trailing newline).
    pub v6text: String,
}

/// One ipf rule object before serialization (mirrors ipfRuleObj's sortObj).
#[derive(Debug, Clone)]
struct SortObj {
    action: String,
    direction: Dir,
    priority: u32,
    protocol: String,
    header: String,
    v4text: Vec<String>,
    v6text: Vec<String>,
    /// Original addresses (for compareAddrs on targets[0]).
    targets: Vec<String>,
    /// Protocol targets rendered as comparison keys.
    proto_first: ProtoFirst,
    /// Owning rule uuid (tracked like node's rulesIncluded).
    #[allow(dead_code)]
    uuid: String,
    all_tags: String,
}

#[derive(Debug, Clone)]
enum ProtoFirst {
    Port(PortTarget),
    Type(String),
    None,
}

/// Options for the compiler.
pub struct CompileOpts<'a> {
    /// VM lookup (local).
    pub all_vms: &'a VmLookup,
    /// Rules (only enabled ones contribute output).
    pub rules: &'a [FwRule],
    /// VMs to generate conf for (typically the affected/enabled set).
    pub vms: &'a [Vm],
    /// True when /dev/ipfev exists (emit smartos_ipf_version 2 + set-tag).
    pub have_ipfev: bool,
    /// Date string for the header banner.
    pub date: String,
}

fn icmpr(protocol: &str) -> bool {
    protocol == "icmp" || protocol == "icmp6"
}

/// protoTarget(): the per-target ipf qualifier text.
fn proto_target(protocol: &str, t: &ProtoFirst) -> String {
    match t {
        ProtoFirst::None => String::new(),
        ProtoFirst::Type(s) => {
            if s == "all" {
                String::new()
            } else {
                let parts: Vec<&str> = s.split(':').collect();
                if parts.len() == 1 {
                    format!("icmp-type {}", parts[0])
                } else {
                    format!("icmp-type {} code {}", parts[0], parts[1])
                }
            }
        }
        ProtoFirst::Port(p) => {
            let _ = protocol;
            match p {
                PortTarget::All => String::new(),
                PortTarget::Range { start, end } => {
                    format!("port {start} : {end}")
                }
                PortTarget::Port(n) => format!("port = {n}"),
            }
        }
    }
}

/// Iterate the rule's protocol targets as ProtoFirst, preserving order.
fn proto_targets_list(rule: &FwRule) -> Vec<ProtoFirst> {
    match &rule.proto_targets {
        ProtoTargets::Ports(ports) => {
            ports.iter().cloned().map(ProtoFirst::Port).collect()
        }
        ProtoTargets::Types(types) => {
            types.iter().cloned().map(ProtoFirst::Type).collect()
        }
        ProtoTargets::All => vec![ProtoFirst::None],
    }
}

/// noRulesNeeded(): FROM+ALLOW and TO+BLOCK are no-ops at priority 0.
fn no_rules_needed(dir: Dir, rule: &FwRule) -> bool {
    if rule.priority != 0 {
        return false;
    }
    (dir == Dir::From && rule.action == "allow")
        || (dir == Dir::To && rule.action == "block")
}

/// ipfRuleObj(): build a SortObj for a direction + concrete target(s).
#[allow(clippy::too_many_arguments)]
fn ipf_rule_obj(
    rule: &FwRule,
    dir: Dir,
    targets: &[String],
    typ: &str,
    value: &str,
    have_ipfev: bool,
) -> SortObj {
    let ipf_proto = if rule.protocol == "icmp6" {
        "ipv6-icmp"
    } else {
        rule.protocol.as_str()
    };

    let mut readtags: Vec<String> = Vec::new();
    if have_ipfev {
        if !rule.uuid.is_empty() {
            readtags.push(format!("uuid={}", rule.uuid));
        }
        if rule.log {
            readtags.push("cfwlog".to_string());
        }
    }
    let all_tags = if !readtags.is_empty() {
        format!(" set-tag({})", readtags.join(", "))
    } else {
        String::new()
    };

    let header = format!(
        "\n# rule={}, version={}, {}={}",
        rule.uuid, rule.version, typ, value
    );

    let pts = proto_targets_list(rule);
    let proto_first = pts.first().cloned().unwrap_or(ProtoFirst::None);

    let mut v4text: Vec<String> = Vec::new();
    let mut v6text: Vec<String> = Vec::new();

    let pass_block = if rule.action == "allow" { "pass" } else { "block" };
    let inout = if dir == Dir::From { "out" } else { "in" };

    if typ == "wildcard" && value == "any" {
        for t in &pts {
            let wild = format!(
                "{} {} quick proto {} from any to any {}",
                pass_block,
                inout,
                ipf_proto,
                proto_target(&rule.protocol, t)
            );
            if rule.protocol != "icmp6" {
                v4text.push(wild.clone());
            }
            if rule.protocol != "icmp" {
                v6text.push(wild);
            }
        }
    } else {
        for target in targets {
            let isv6 = target.contains(':');
            // Suppress ICMPv4/IPv6 and ICMPv6/IPv4 mismatches.
            if (isv6 && rule.protocol == "icmp")
                || (!isv6 && rule.protocol == "icmp6")
            {
                continue;
            }
            for t in &pts {
                let line = format!(
                    "{} {} quick proto {} from {} to {} {}",
                    pass_block,
                    inout,
                    ipf_proto,
                    if dir == Dir::To { target.as_str() } else { "any" },
                    if dir == Dir::To { "any" } else { target.as_str() },
                    proto_target(&rule.protocol, t)
                );
                if isv6 {
                    v6text.push(line);
                } else {
                    v4text.push(line);
                }
            }
        }
    }

    SortObj {
        action: rule.action.clone(),
        direction: dir,
        priority: rule.priority,
        protocol: rule.protocol.clone(),
        header,
        v4text,
        v6text,
        targets: targets.to_vec(),
        proto_first,
        uuid: rule.uuid.clone(),
        all_tags,
    }
}

/// vmsOnSide(): UUIDs of local VMs targeted on the given side.
fn vms_on_side(all_vms: &VmLookup, rule: &FwRule, dir: Dir) -> Vec<String> {
    let side = side_for(rule, dir);
    let mut matching = Vec::new();

    // vms
    for uuid in &side.vms {
        if let Some(vm) = all_vms.all.get(uuid) {
            if owner_ok(rule, vm) {
                matching.push(uuid.clone());
            }
        }
    }
    // tags
    for tag in &side.tags {
        match tag {
            TagEntry::Name(name) => {
                if let Some(m) = all_vms.tags.get(name) {
                    for (uuid, vm) in m {
                        if owner_ok(rule, vm) {
                            matching.push(uuid.clone());
                        }
                    }
                }
            }
            TagEntry::NameValue(name, val) => {
                if let Some(vm) =
                    all_vms.tag_values.get(name).and_then(|m| m.get(val))
                {
                    for (uuid, v) in vm {
                        if owner_ok(rule, v) {
                            matching.push(uuid.clone());
                        }
                    }
                }
            }
        }
    }
    // wildcards (vmall -> all VMs; any -> skip)
    for w in &side.wildcards {
        if w == "any" {
            continue;
        }
        if w == "vmall" {
            for (uuid, vm) in &all_vms.all {
                if owner_ok(rule, vm) {
                    matching.push(uuid.clone());
                }
            }
        }
    }

    matching
}

fn owner_ok(rule: &FwRule, vm: &Vm) -> bool {
    match &rule.owner_uuid {
        Some(o) => vm.owner_uuid.as_deref() == Some(o.as_str()),
        None => true,
    }
}

fn side_for(rule: &FwRule, dir: Dir) -> &Side {
    match dir {
        Dir::From => &rule.from,
        Dir::To => &rule.to,
    }
}

/// rulesFromOtherSide(): produce ipf rule objects for the opposite side.
fn rules_from_other_side(
    rule: &FwRule,
    dir: Dir,
    local_vms: &VmLookup,
    have_ipfev: bool,
) -> Vec<SortObj> {
    let other = match dir {
        Dir::From => Dir::To,
        Dir::To => Dir::From,
    };
    let oside = side_for(rule, other);
    let mut out = Vec::new();

    if oside.wildcards.iter().any(|w| w == "any") {
        out.push(ipf_rule_obj(
            rule,
            dir,
            &["0.0.0.0".to_string()],
            "wildcard",
            "any",
            have_ipfev,
        ));
        return out;
    }

    // IPs and subnets: literal, as-is.
    for ip in &oside.ips {
        out.push(ipf_rule_obj(rule, dir, &[ip.clone()], "ip", ip, have_ipfev));
    }
    for sn in &oside.subnets {
        out.push(ipf_rule_obj(
            rule,
            dir,
            &[sn.clone()],
            "subnet",
            sn,
            have_ipfev,
        ));
    }

    // tag/vm/wildcard: expand to each VM's IPs (skip VMs with 0 ips).
    // tags
    for tag in &oside.tags {
        match tag {
            TagEntry::Name(name) => {
                if let Some(m) = local_vms.tags.get(name) {
                    for vm in m.values() {
                        if !owner_ok(rule, vm) || vm.ips.is_empty() {
                            continue;
                        }
                        out.push(ipf_rule_obj(
                            rule, dir, &vm.ips, "tag", name, have_ipfev,
                        ));
                    }
                }
            }
            TagEntry::NameValue(name, val) => {
                if let Some(m) =
                    local_vms.tag_values.get(name).and_then(|mm| mm.get(val))
                {
                    for vm in m.values() {
                        if !owner_ok(rule, vm) || vm.ips.is_empty() {
                            continue;
                        }
                        out.push(ipf_rule_obj(
                            rule, dir, &vm.ips, "tagValue", name, have_ipfev,
                        ));
                    }
                }
            }
        }
    }
    // vms
    for uuid in &oside.vms {
        if let Some(vm) = local_vms.all.get(uuid) {
            if !owner_ok(rule, vm) || vm.ips.is_empty() {
                continue;
            }
            out.push(ipf_rule_obj(rule, dir, &vm.ips, "vm", uuid, have_ipfev));
        }
    }
    // wildcards (vmall expands to all VMs; any handled above)
    for w in &oside.wildcards {
        if w == "any" {
            continue;
        }
        if w == "vmall" {
            for vm in local_vms.all.values() {
                if !owner_ok(rule, vm) || vm.ips.is_empty() {
                    continue;
                }
                out.push(ipf_rule_obj(
                    rule, dir, &vm.ips, "wildcard", w, have_ipfev,
                ));
            }
        }
    }

    out
}

// --- ordering (compareRules) ---

fn compare_ports(p1: &PortTarget, p2: &PortTarget) -> Ordering {
    let key = |p: &PortTarget| match p {
        PortTarget::All => i64::MIN,
        PortTarget::Port(n) => *n as i64,
        PortTarget::Range { start, .. } => *start as i64,
    };
    // "all" before numbers (i64::MIN achieves that).
    key(p1).cmp(&key(p2))
}

fn compare_types(t1: &str, t2: &str) -> Ordering {
    if t1 == "all" {
        return if t2 == "all" { Ordering::Equal } else { Ordering::Less };
    }
    if t2 == "all" {
        return Ordering::Greater;
    }
    let p1: Vec<&str> = t1.split(':').collect();
    let p2: Vec<&str> = t2.split(':').collect();
    let a0: i64 = p1[0].parse().unwrap_or(0);
    let b0: i64 = p2[0].parse().unwrap_or(0);
    match a0.cmp(&b0) {
        Ordering::Equal => {}
        o => return o,
    }
    match (p1.len(), p2.len()) {
        (1, 1) => Ordering::Equal,
        (1, _) => Ordering::Less,
        (_, 1) => Ordering::Greater,
        _ => {
            let a1: i64 = p1[1].parse().unwrap_or(0);
            let b1: i64 = p2[1].parse().unwrap_or(0);
            a1.cmp(&b1)
        }
    }
}

fn compare_addrs(a1: &str, a2: &str) -> Ordering {
    let s1 = a1.split('/').next().unwrap_or(a1);
    let s2 = a2.split('/').next().unwrap_or(a2);
    crate::vm::addr_compare(s1, s2)
}

fn compare_rules(r1: &SortObj, r2: &SortObj) -> Ordering {
    // protocol
    match r1.protocol.cmp(&r2.protocol) {
        Ordering::Equal => {}
        o => return o,
    }
    // direction ("from" < "to" lexically)
    match r1.direction.as_str().cmp(r2.direction.as_str()) {
        Ordering::Equal => {}
        o => return o,
    }
    // priority DESC
    match r1.priority.cmp(&r2.priority) {
        Ordering::Equal => {}
        o => return o.reverse(),
    }
    // action: outbound (from) blocks before allows; inbound (to) allows
    // before blocks.
    let allow_block = r1.action == "allow" && r2.action == "block";
    let block_allow = r1.action == "block" && r2.action == "allow";
    if r1.direction == Dir::From {
        if allow_block {
            return Ordering::Greater;
        }
        if block_allow {
            return Ordering::Less;
        }
    } else {
        if allow_block {
            return Ordering::Less;
        }
        if block_allow {
            return Ordering::Greater;
        }
    }
    // ports/types
    let res = if icmpr(&r1.protocol) {
        let t1 = match &r1.proto_first {
            ProtoFirst::Type(s) => s.as_str(),
            _ => "",
        };
        let t2 = match &r2.proto_first {
            ProtoFirst::Type(s) => s.as_str(),
            _ => "",
        };
        compare_types(t1, t2)
    } else {
        let dummy = PortTarget::All;
        let p1 = match &r1.proto_first {
            ProtoFirst::Port(p) => p,
            _ => &dummy,
        };
        let p2 = match &r2.proto_first {
            ProtoFirst::Port(p) => p,
            _ => &dummy,
        };
        compare_ports(p1, p2)
    };
    if res != Ordering::Equal {
        return res;
    }
    // target IPs
    let a1 = r1.targets.first().map(String::as_str).unwrap_or("");
    let a2 = r2.targets.first().map(String::as_str).unwrap_or("");
    compare_addrs(a1, a2)
}

// --- fallbacks ---

fn fallbacks_base() -> Vec<&'static str> {
    vec![
        "",
        "# fwadm fallbacks",
        "block in all",
        "pass out quick proto tcp from any to any flags S/SA keep state",
        "pass out proto tcp from any to any",
        "pass out proto udp from any to any keep state",
    ]
}

fn v4_fallbacks() -> Vec<String> {
    let mut f: Vec<String> =
        fallbacks_base().into_iter().map(String::from).collect();
    f.push("pass out quick proto icmp from any to any keep state".into());
    f.push("pass out proto icmp from any to any".into());
    f
}

fn v6_fallbacks() -> Vec<String> {
    let mut f: Vec<String> =
        fallbacks_base().into_iter().map(String::from).collect();
    f.push("pass out quick proto ipv6-icmp from any to any keep state".into());
    f.push("pass out proto ipv6-icmp from any to any".into());
    f
}

/// The UUIDs of local VMs affected by a rule (union of both sides). Mirrors
/// `fw.vms` (which calls vmsOnSide for both directions and dedups).
pub fn vms_affected(all_vms: &VmLookup, rule: &FwRule) -> Vec<String> {
    let mut v = vms_on_side(all_vms, rule, Dir::From);
    v.extend(vms_on_side(all_vms, rule, Dir::To));
    v.sort();
    v.dedup();
    v
}

/// prepareIPFdata(): build the per-zone ipf config files.
pub fn prepare_ipf_data(opts: &CompileOpts) -> Vec<IpfConf> {
    // conf: VMs whose firewall is enabled get an (initially empty) rule list.
    let mut conf: BTreeMap<String, Vec<SortObj>> = BTreeMap::new();
    for vm in opts.vms {
        if let Some(v) = opts.all_vms.all.get(&vm.uuid) {
            if v.enabled {
                conf.insert(vm.uuid.clone(), Vec::new());
            }
        }
    }

    // targetVMs[i] = { from, to } per enabled rule.
    let target_vms: Vec<Option<(Vec<String>, Vec<String>)>> = opts
        .rules
        .iter()
        .map(|rule| {
            if !rule.enabled {
                None
            } else {
                Some((
                    vms_on_side(opts.all_vms, rule, Dir::From),
                    vms_on_side(opts.all_vms, rule, Dir::To),
                ))
            }
        })
        .collect();

    // keepInboundState: for blocked outbound protocols, track inbound state.
    let mut keep_inbound: BTreeMap<String, BTreeMap<String, bool>> =
        BTreeMap::new();
    for (i, rule) in opts.rules.iter().enumerate() {
        if !rule.enabled || rule.action == "allow" {
            continue;
        }
        if let Some((from, _)) = &target_vms[i] {
            for uuid in from {
                keep_inbound
                    .entry(uuid.clone())
                    .or_default()
                    .insert(rule.protocol.clone(), true);
            }
        }
    }

    // For each enabled rule, both directions, append the other-side rules to
    // each targeted VM's conf.
    for (i, rule) in opts.rules.iter().enumerate() {
        if !rule.enabled {
            continue;
        }
        for dir in [Dir::From, Dir::To] {
            if no_rules_needed(dir, rule) {
                continue;
            }
            let other_side =
                rules_from_other_side(rule, dir, opts.all_vms, opts.have_ipfev);
            if let Some((from, to)) = &target_vms[i] {
                let side_list = if dir == Dir::From { from } else { to };
                for uuid in side_list {
                    let enabled = opts
                        .all_vms
                        .all
                        .get(uuid)
                        .map(|v| v.enabled)
                        .unwrap_or(false);
                    if !enabled || !conf.contains_key(uuid) {
                        continue;
                    }
                    if let Some(list) = conf.get_mut(uuid) {
                        list.extend(other_side.iter().cloned());
                    }
                }
            }
        }
    }

    // Render each VM's conf.
    let mut out = Vec::new();
    for (vm, mut sort_objs) in conf {
        let header = build_header(&opts.date, opts.have_ipfev);
        let mut ipf4 = header.clone();
        let mut ipf6 = header.clone();

        let iks = keep_inbound.get(&vm).cloned().unwrap_or_default();

        sort_objs.sort_by(compare_rules);

        for so in &sort_objs {
            let mut ktxt = KEEP_FRAGS.to_string();
            let add_state = !so.all_tags.is_empty()
                || (so.direction == Dir::From && so.action == "allow")
                || (so.direction == Dir::To
                    && iks.get(&so.protocol).copied().unwrap_or(false));
            if add_state {
                ktxt.push_str(KEEP_STATE);
                ktxt.push_str(&so.all_tags);
            }

            ipf4.push(so.header.clone());
            ipf6.push(so.header.clone());
            for line in &so.v4text {
                ipf4.push(format!("{line}{ktxt}"));
            }
            for line in &so.v6text {
                ipf6.push(format!("{line}{ktxt}"));
            }
        }

        let mut v4 = ipf4;
        v4.extend(v4_fallbacks());
        let mut v6 = ipf6;
        v6.extend(v6_fallbacks());

        let zonepath = opts
            .all_vms
            .all
            .get(&vm)
            .map(|v| v.zonepath.clone())
            .unwrap_or_default();

        out.push(IpfConf {
            uuid: vm,
            zonepath,
            v4text: format!("{}\n", v4.join("\n")),
            v6text: format!("{}\n", v6.join("\n")),
        });
    }

    out
}

fn build_header(date: &str, have_ipfev: bool) -> Vec<String> {
    let mut h = vec![
        "# DO NOT EDIT THIS FILE. THIS FILE IS AUTO-GENERATED BY fwadm(8)"
            .to_string(),
        "# AND MAY BE OVERWRITTEN AT ANY TIME.".to_string(),
        "#".to_string(),
        format!("# File generated at {date}"),
        "#".to_string(),
    ];
    if have_ipfev {
        h.push("# smartos_ipf_version 2".to_string());
    }
    h.push("#".to_string());
    h.push(String::new());
    h
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::RuleData;
    use crate::parse::ParseOpts;

    fn mk_rule(text: &str, enabled: bool, owner: Option<&str>) -> FwRule {
        FwRule::create(
            RuleData {
                rule: Some(text.to_string()),
                enabled: Some(enabled),
                owner_uuid: owner.map(String::from),
                ..Default::default()
            },
            ParseOpts::default(),
        )
        .expect("rule")
    }

    fn mk_vm(uuid: &str, ip: &str, tag: Option<(&str, &str)>) -> Vm {
        let mut tags = BTreeMap::new();
        if let Some((k, v)) = tag {
            tags.insert(k.to_string(), v.to_string());
        }
        Vm {
            uuid: uuid.to_string(),
            enabled: true,
            owner_uuid: Some("11111111-1111-1111-1111-111111111111".into()),
            state: Some("running".into()),
            zonepath: format!("/zones/{uuid}"),
            tags,
            ips: vec![ip.to_string()],
        }
    }

    #[test]
    fn tcp_inbound_allow_from_ip() {
        let vm = mk_vm(
            "aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa",
            "10.0.0.5",
            None,
        );
        let lk = VmLookup::build(vec![vm.clone()]);
        let rule = mk_rule(
            &format!(
                "FROM ip 1.2.3.4 TO vm {} ALLOW tcp PORT 22",
                vm.uuid
            ),
            true,
            None,
        );
        let opts = CompileOpts {
            all_vms: &lk,
            rules: std::slice::from_ref(&rule),
            vms: std::slice::from_ref(&vm),
            have_ipfev: false,
            date: "Mon".into(),
        };
        let confs = prepare_ipf_data(&opts);
        assert_eq!(confs.len(), 1);
        let c = &confs[0];
        assert!(c.v4text.contains(
            "pass in quick proto tcp from 1.2.3.4 to any port = 22 keep frags"
        ));
        // inbound allow with no outbound block => no keep state.
        assert!(!c.v4text.contains("port = 22 keep frags keep state"));
    }

    #[test]
    fn fallbacks_present() {
        let vm = mk_vm(
            "aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa",
            "10.0.0.5",
            None,
        );
        let lk = VmLookup::build(vec![vm.clone()]);
        let rule = mk_rule(
            &format!("FROM ip 1.2.3.4 TO vm {} ALLOW tcp PORT 22", vm.uuid),
            true,
            None,
        );
        let opts = CompileOpts {
            all_vms: &lk,
            rules: std::slice::from_ref(&rule),
            vms: std::slice::from_ref(&vm),
            have_ipfev: false,
            date: "Mon".into(),
        };
        let c = &prepare_ipf_data(&opts)[0];
        assert!(c.v4text.contains("block in all"));
        assert!(c.v4text.contains("pass out proto icmp from any to any"));
        assert!(c.v6text.contains("pass out proto ipv6-icmp from any to any"));
    }

    #[test]
    fn ipfev_set_tag() {
        let vm = mk_vm(
            "aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa",
            "10.0.0.5",
            None,
        );
        let lk = VmLookup::build(vec![vm.clone()]);
        let rule = mk_rule(
            &format!("FROM ip 1.2.3.4 TO vm {} ALLOW tcp PORT 22", vm.uuid),
            true,
            None,
        );
        let opts = CompileOpts {
            all_vms: &lk,
            rules: std::slice::from_ref(&rule),
            vms: std::slice::from_ref(&vm),
            have_ipfev: true,
            date: "Mon".into(),
        };
        let c = &prepare_ipf_data(&opts)[0];
        assert!(c.v4text.contains("# smartos_ipf_version 2"));
        assert!(c.v4text.contains(&format!("set-tag(uuid={})", rule.uuid)));
    }
}
