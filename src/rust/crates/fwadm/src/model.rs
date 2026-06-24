// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.
//
// Copyright 2026 Edgecast Cloud LLC.

//! Firewall rule object model. Faithful port of
//! `src/fw/node_modules/fwrule/rule.js` (FwRule): construction, dedup/sort of
//! targets and protocol targets, the canonical `text()` serializer (byte-exact
//! with node), and JSON `serialize()`.

use std::collections::BTreeMap;

use serde_json::{Map, Value};

use crate::error::FwError;
use crate::parse::{
    self, IcmpTarget, ParseOpts, ParsedProtocol, ParsedRule, PortTarget,
    Target,
};

/// Maximum number of targets per side.
const MAX_TARGETS_PER_SIDE: usize = 24;
/// Maximum number of protocol targets (v>=4).
const MAX_PROTOCOL_TARGETS: usize = 24;
/// Minimum version for the larger protocol-target list.
const MINVER_LGPROTOTARG: u32 = 4;
/// The old maximum number of protocol targets (v<4).
const OLD_MAX_PORTS: usize = 8;

/// One side's targets, grouped by type, deduped and sorted (matches
/// `this.from`/`this.to`). Tags are stored as a list of either bare names or
/// (name, value) pairs.
#[derive(Debug, Clone, Default)]
pub struct Side {
    /// IP addresses (sorted lexically).
    pub ips: Vec<String>,
    /// Subnets (sorted lexically).
    pub subnets: Vec<String>,
    /// Tags: `Ok(name)` for tag-without-value, `Err((name, value))` for tag=val.
    pub tags: Vec<TagEntry>,
    /// VM UUIDs (sorted lexically).
    pub vms: Vec<String>,
    /// Wildcards ("any" / "vmall"), sorted lexically.
    pub wildcards: Vec<String>,
}

/// A tag entry: either a bare name (matches all values) or a name=value pair.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TagEntry {
    /// `tag <name>` (no value).
    Name(String),
    /// `tag <name> = <value>`.
    NameValue(String, String),
}

/// The protocol targets in canonical (sorted) order.
#[derive(Debug, Clone)]
pub enum ProtoTargets {
    /// TCP/UDP ports.
    Ports(Vec<PortTarget>),
    /// ICMP types, stored as the node string form ("N", "N:C", or "all").
    Types(Vec<String>),
    /// AH/ESP: implicit `all` (one target).
    All,
}

/// A constructed firewall rule, equivalent to the FwRule object.
#[derive(Debug, Clone)]
pub struct FwRule {
    /// Rule UUID.
    pub uuid: String,
    /// Version string ("<ms>.<pid6>").
    pub version: String,
    /// Owner UUID (absent => global rule).
    pub owner_uuid: Option<String>,
    /// Global flag (true when no owner_uuid).
    pub global: bool,
    /// Enabled flag.
    pub enabled: bool,
    /// Logging flag.
    pub log: bool,
    /// Optional created_by.
    pub created_by: Option<String>,
    /// Optional description.
    pub description: Option<String>,
    /// "allow" or "block".
    pub action: String,
    /// Priority (0..100).
    pub priority: u32,
    /// Protocol name: tcp/udp/icmp/icmp6/ah/esp.
    pub protocol: String,
    /// Protocol targets (ports/types/all).
    pub proto_targets: ProtoTargets,
    /// `from` side.
    pub from: Side,
    /// `to` side.
    pub to: Side,
    /// True if a `vmall` wildcard is present on either side.
    pub all_vms: bool,
}

/// Input data for constructing a rule (the JSON payload fields).
#[derive(Debug, Clone, Default)]
pub struct RuleData {
    /// Raw rule DSL text.
    pub rule: Option<String>,
    /// Pre-parsed AST (used by tests / internal paths).
    pub parsed: Option<ParsedRule>,
    /// Rule UUID.
    pub uuid: Option<String>,
    /// Version string.
    pub version: Option<String>,
    /// Owner UUID.
    pub owner_uuid: Option<String>,
    /// Enabled flag.
    pub enabled: Option<bool>,
    /// Log flag.
    pub log: Option<bool>,
    /// created_by.
    pub created_by: Option<String>,
    /// description.
    pub description: Option<String>,
}

/// Returns true if the string is a valid lowercase-hex UUID (8-4-4-4-12).
pub fn is_uuid(s: &str) -> bool {
    let b = s.as_bytes();
    if b.len() != 36 {
        return false;
    }
    let dashes = [8, 13, 18, 23];
    for (i, &c) in b.iter().enumerate() {
        if dashes.contains(&i) {
            if c != b'-' {
                return false;
            }
        } else if !(c.is_ascii_digit() || (b'a'..=b'f').contains(&c)) {
            return false;
        }
    }
    true
}

// --- escaping (rule.js tagEscape / quote) ---

/// Escape and quote a tag string for canonical output. Mirrors `quote()` and
/// the `unescapes` map in rule.js.
pub fn quote(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for ch in s.chars() {
        match unescape_char(ch) {
            Some(esc) => {
                out.push('\\');
                out.push_str(esc);
            }
            None => out.push(ch),
        }
    }
    out.push('"');
    out
}

/// Maps a char to its escape body per the `unescapes` table; None if it does
/// not need escaping.
fn unescape_char(ch: char) -> Option<&'static str> {
    match ch {
        '"' => Some("\""),
        '(' => Some("("),
        ')' => Some(")"),
        '\\' => Some("\\"),
        '\u{0008}' => Some("b"),
        '\u{0009}' => Some("t"),
        '\u{000A}' => Some("n"),
        '\u{000C}' => Some("f"),
        '\u{000D}' => Some("r"),
        '\u{0000}' => Some("u0000"),
        '\u{0001}' => Some("u0001"),
        '\u{0002}' => Some("u0002"),
        '\u{0003}' => Some("u0003"),
        '\u{0004}' => Some("u0004"),
        '\u{0005}' => Some("u0005"),
        '\u{0006}' => Some("u0006"),
        '\u{0007}' => Some("u0007"),
        '\u{000B}' => Some("u000B"),
        '\u{000E}' => Some("u000E"),
        '\u{000F}' => Some("u000F"),
        '\u{0010}' => Some("u0010"),
        '\u{0011}' => Some("u0011"),
        '\u{0012}' => Some("u0012"),
        '\u{0013}' => Some("u0013"),
        '\u{0014}' => Some("u0014"),
        '\u{0015}' => Some("u0015"),
        '\u{0016}' => Some("u0016"),
        '\u{0017}' => Some("u0017"),
        '\u{0018}' => Some("u0018"),
        '\u{0019}' => Some("u0019"),
        '\u{001A}' => Some("u001A"),
        '\u{001B}' => Some("u001B"),
        '\u{001C}' => Some("u001C"),
        '\u{001D}' => Some("u001D"),
        '\u{001E}' => Some("u001E"),
        '\u{001F}' => Some("u001F"),
        '\u{007F}' => Some("u007F"),
        '\u{0085}' => Some("u0085"),
        '\u{00A0}' => Some("u00A0"),
        '\u{1680}' => Some("u1680"),
        '\u{180E}' => Some("u180E"),
        '\u{2000}' => Some("u2000"),
        '\u{2001}' => Some("u2001"),
        '\u{2002}' => Some("u2002"),
        '\u{2003}' => Some("u2003"),
        '\u{2004}' => Some("u2004"),
        '\u{2005}' => Some("u2005"),
        '\u{2006}' => Some("u2006"),
        '\u{2007}' => Some("u2007"),
        '\u{2008}' => Some("u2008"),
        '\u{2009}' => Some("u2009"),
        '\u{200A}' => Some("u200A"),
        '\u{200B}' => Some("u200B"),
        '\u{200C}' => Some("u200C"),
        '\u{200D}' => Some("u200D"),
        '\u{2028}' => Some("u2028"),
        '\u{2029}' => Some("u2029"),
        '\u{202F}' => Some("u202F"),
        '\u{205F}' => Some("u205F"),
        '\u{2060}' => Some("u2060"),
        '\u{3000}' => Some("u3000"),
        '\u{FEFF}' => Some("uFEFF"),
        _ => None,
    }
}

// --- ICMP sort key (rule.js icmpTypeSort) ---

/// Sort key for an ICMP type string: `(type << 8) + code`. Reproduces the
/// node icmpTypeSort comparator, including its handling of bare types.
fn icmp_sort_key(s: &str) -> i64 {
    if s == "all" {
        return -1; // "all" sorts before numeric (compareTypes); not used here
    }
    let parts: Vec<&str> = s.split(':').collect();
    let ty: i64 = parts[0].parse().unwrap_or(0);
    let code: i64 =
        if parts.len() == 1 { 0 } else { parts[1].parse().unwrap_or(0) };
    (ty << 8) + code
}

impl FwRule {
    /// Construct a rule from data, mirroring the FwRule constructor's
    /// validation and translation. `opts` controls parse + version caps.
    pub fn create(data: RuleData, opts: ParseOpts) -> Result<FwRule, FwError> {
        let mut errs: Vec<FwError> = Vec::new();

        // Parse the rule text (or accept a pre-parsed AST).
        let parsed: Option<ParsedRule> = if data.parsed.is_some() {
            data.parsed.clone()
        } else if let Some(text) = &data.rule {
            match parse::parse(text, opts) {
                Ok(p) => Some(p),
                Err(e) => {
                    errs.push(e);
                    None
                }
            }
        } else {
            errs.push(FwError::rule("No rule specified"));
            None
        };

        // uuid
        let uuid = match &data.uuid {
            Some(u) => {
                if !is_uuid(u) {
                    errs.push(FwError::param("uuid", "Invalid rule UUID"));
                }
                u.clone()
            }
            None => uuid::Uuid::new_v4().to_string(),
        };

        let version = data.version.clone().unwrap_or_else(generate_version);

        let mut global = false;
        let owner_uuid = match &data.owner_uuid {
            Some(o) => {
                if !is_uuid(o) {
                    errs.push(FwError::param(
                        "owner_uuid",
                        "Invalid owner UUID",
                    ));
                }
                Some(o.clone())
            }
            None => {
                global = true;
                None
            }
        };

        let enabled = data.enabled.unwrap_or(false);
        let log = data.log.unwrap_or(false);

        if let Some(s) = &data.created_by {
            if s.len() > 255 {
                errs.push(FwError::param(
                    "created_by",
                    "created_by must be shorter than 255 characters",
                ));
            }
        }
        if let Some(s) = &data.description {
            if s.len() > 255 {
                errs.push(FwError::param(
                    "description",
                    "description must be shorter than 255 characters",
                ));
            }
        }

        // If parsing failed, bail with the accumulated errors now (node throws
        // before the translate step when errs is non-empty).
        let parsed = match parsed {
            Some(p) if errs.is_empty() => p,
            _ => return Err(FwError::multi(errs)),
        };

        // --- translate ---
        let action = parsed.action.clone();
        let priority = parsed.priority;
        let protocol = parsed.protocol.name().to_string();

        let proto_targets = match &parsed.protocol {
            ParsedProtocol::Icmp(t) | ParsedProtocol::Icmp6(t) => {
                let mut types: Vec<String> =
                    t.iter().map(icmp_to_string).collect();
                types.sort_by_key(|s| icmp_sort_key(s));
                ProtoTargets::Types(types)
            }
            ParsedProtocol::Ah | ParsedProtocol::Esp => ProtoTargets::All,
            ParsedProtocol::Tcp(p) | ParsedProtocol::Udp(p) => {
                let mut ports = p.clone();
                ports.sort_by_key(port_sort_key);
                ProtoTargets::Ports(ports)
            }
        };

        let pt_len = match &proto_targets {
            ProtoTargets::Ports(v) => v.len(),
            ProtoTargets::Types(v) => v.len(),
            ProtoTargets::All => 1,
        };
        let is_icmp = protocol == "icmp" || protocol == "icmp6";
        if opts.max_version < MINVER_LGPROTOTARG {
            if pt_len > OLD_MAX_PORTS {
                return Err(FwError::rule(format!(
                    "maximum of {OLD_MAX_PORTS} {} allowed",
                    if is_icmp { "types" } else { "ports" }
                )));
            }
        } else if pt_len > MAX_PROTOCOL_TARGETS {
            return Err(FwError::rule(format!(
                "maximum of {MAX_PROTOCOL_TARGETS} {} allowed",
                if is_icmp { "types" } else { "ports" }
            )));
        }

        let from = build_side(&parsed.from)?;
        let to = build_side(&parsed.to)?;

        // Per-side target count cap (node counts raw targets before dedup).
        if parsed.from.len() > MAX_TARGETS_PER_SIDE
            || parsed.to.len() > MAX_TARGETS_PER_SIDE
        {
            return Err(FwError::rule(format!(
                "maximum of {MAX_TARGETS_PER_SIDE} targets allowed per side"
            )));
        }

        let all_vms = from.wildcards.iter().any(|w| w == "vmall")
            || to.wildcards.iter().any(|w| w == "vmall");

        // ICMP address-family sanity checks.
        if protocol == "icmp" {
            for ip in from.ips.iter().chain(to.ips.iter()) {
                if !is_ipv4(ip) {
                    return Err(FwError::rule(
                        "rule affects ICMPv4 but contains a non-IPv4 address",
                    ));
                }
            }
            for sn in from.subnets.iter().chain(to.subnets.iter()) {
                let head = sn.split('/').next().unwrap_or("");
                if !is_ipv4(head) {
                    return Err(FwError::rule(
                        "rule affects ICMPv4 but contains a non-IPv4 subnet",
                    ));
                }
            }
        } else if protocol == "icmp6" {
            for ip in from.ips.iter().chain(to.ips.iter()) {
                if !is_ipv6(ip) {
                    return Err(FwError::rule(
                        "rule affects ICMPv6 but contains a non-IPv6 address",
                    ));
                }
            }
            for sn in from.subnets.iter().chain(to.subnets.iter()) {
                let head = sn.split('/').next().unwrap_or("");
                if !is_ipv6(head) {
                    return Err(FwError::rule(
                        "rule affects ICMPv6 but contains a non-IPv6 subnet",
                    ));
                }
            }
        }

        // Final: must affect VMs.
        let has_tags =
            !from.tags.is_empty() || !to.tags.is_empty();
        let has_vms = !from.vms.is_empty() || !to.vms.is_empty();
        if !all_vms && !has_tags && !has_vms {
            return Err(FwError::rule("rule does not affect VMs"));
        }

        Ok(FwRule {
            uuid,
            version,
            owner_uuid,
            global,
            enabled,
            log,
            created_by: data.created_by,
            description: data.description,
            action,
            priority,
            protocol,
            proto_targets,
            from,
            to,
            all_vms,
        })
    }

    /// Canonical DSL text, byte-exact with `FwRule.prototype.text()`.
    pub fn text(&self) -> String {
        let from_txt = side_text(&self.from);
        let to_txt = side_text(&self.to);

        let proto_txt = self.proto_text();

        let prio_txt = if self.priority > 0 {
            format!(" PRIORITY {}", self.priority)
        } else {
            String::new()
        };

        let wrap = |parts: &[String]| -> String {
            if parts.len() > 1 {
                format!("({})", parts.join(" OR "))
            } else {
                parts.join(" OR ")
            }
        };

        format!(
            "FROM {} TO {} {} {}{}{}",
            wrap(&from_txt),
            wrap(&to_txt),
            self.action.to_uppercase(),
            self.protocol,
            proto_txt,
            prio_txt
        )
    }

    fn proto_text(&self) -> String {
        match &self.proto_targets {
            ProtoTargets::Types(types) => {
                let inner: Vec<String> = types
                    .iter()
                    .map(|t| {
                        if t == "all" {
                            "all".to_string()
                        } else {
                            let parts: Vec<&str> = t.split(':').collect();
                            if parts.len() == 1 {
                                parts[0].to_string()
                            } else {
                                format!("{} CODE {}", parts[0], parts[1])
                            }
                        }
                    })
                    .collect();
                let joined = inner.join(" AND TYPE ");
                if types.len() > 1 {
                    format!(" (TYPE {joined})")
                } else {
                    format!(" TYPE {joined}")
                }
            }
            ProtoTargets::All => String::new(),
            ProtoTargets::Ports(ports) => {
                let mut contains_range = false;
                let rendered: Vec<String> = ports
                    .iter()
                    .map(|p| match p {
                        PortTarget::Range { start, end } => {
                            contains_range = true;
                            format!("{start} - {end}")
                        }
                        PortTarget::Port(n) => n.to_string(),
                        PortTarget::All => "all".to_string(),
                    })
                    .collect();
                if contains_range {
                    format!(" PORTS {}", rendered.join(", "))
                } else if rendered.len() > 1 {
                    format!(" (PORT {})", rendered.join(" AND PORT "))
                } else {
                    format!(" PORT {}", rendered.join(" AND PORT "))
                }
            }
        }
    }

    /// JSON serialization matching `serialize()` (the on-disk and `list -j`
    /// shape). Field order matches the FIELDS array; `global` only present when
    /// true, optional fields omitted when absent.
    pub fn serialize(&self) -> Value {
        let mut m = Map::new();
        // FIELDS order: created_by, description, enabled, global, log,
        // owner_uuid, rule, uuid, version
        if let Some(s) = &self.created_by {
            m.insert("created_by".into(), Value::String(s.clone()));
        }
        if let Some(s) = &self.description {
            m.insert("description".into(), Value::String(s.clone()));
        }
        m.insert("enabled".into(), Value::Bool(self.enabled));
        if self.global {
            m.insert("global".into(), Value::Bool(true));
        }
        m.insert("log".into(), Value::Bool(self.log));
        if let Some(o) = &self.owner_uuid {
            m.insert("owner_uuid".into(), Value::String(o.clone()));
        }
        m.insert("rule".into(), Value::String(self.text()));
        m.insert("uuid".into(), Value::String(self.uuid.clone()));
        m.insert("version".into(), Value::String(self.version.clone()));
        Value::Object(m)
    }
}

/// Convert an IcmpTarget to its node string form.
fn icmp_to_string(t: &IcmpTarget) -> String {
    match t {
        IcmpTarget::All => "all".to_string(),
        IcmpTarget::Type(n) => n.to_string(),
        IcmpTarget::TypeCode(n, c) => format!("{n}:{c}"),
    }
}

/// Sort key for a port target (numeric by start; "all"/range start).
fn port_sort_key(p: &PortTarget) -> i64 {
    match p {
        PortTarget::All => i64::MIN, // unused for tcp/udp in practice
        PortTarget::Port(n) => *n as i64,
        PortTarget::Range { start, .. } => *start as i64,
    }
}

/// Build a deduped/sorted Side from raw parsed targets, mirroring the node
/// constructor's per-direction grouping + sort.
fn build_side(targets: &[Target]) -> Result<Side, FwError> {
    let mut ips: BTreeMap<String, ()> = BTreeMap::new();
    let mut subnets: BTreeMap<String, ()> = BTreeMap::new();
    let mut vms: BTreeMap<String, ()> = BTreeMap::new();
    let mut wildcards: BTreeMap<String, ()> = BTreeMap::new();
    // tag name -> all-flag and set of values (BTreeMap keeps sort order).
    let mut tags: BTreeMap<String, TagAcc> = BTreeMap::new();

    for t in targets {
        match t {
            Target::Ip(a) => {
                ips.insert(a.clone(), ());
            }
            Target::Subnet(a) => {
                subnets.insert(a.clone(), ());
            }
            Target::Vm(u) => {
                vms.insert(u.clone(), ());
            }
            Target::Wildcard(w) => {
                wildcards.insert(w.clone(), ());
            }
            Target::Tag(name) => {
                tags.entry(name.clone()).or_default().all = true;
            }
            Target::TagValue(name, val) => {
                tags.entry(name.clone())
                    .or_default()
                    .values
                    .insert(val.clone(), ());
            }
        }
    }

    Ok(Side {
        ips: ips.into_keys().collect(),
        subnets: subnets.into_keys().collect(),
        vms: vms.into_keys().collect(),
        wildcards: wildcards.into_keys().collect(),
        tags: build_tag_list(tags),
    })
}

#[derive(Default)]
struct TagAcc {
    all: bool,
    values: BTreeMap<String, ()>,
}

/// Mirrors `tagList()`: tags sorted by name; a tag with `all` emits a bare
/// name; otherwise each value emits a (name, value) entry sorted by value.
fn build_tag_list(tags: BTreeMap<String, TagAcc>) -> Vec<TagEntry> {
    let mut out = Vec::new();
    for (name, acc) in tags {
        if acc.all {
            out.push(TagEntry::Name(name));
        } else {
            for val in acc.values.into_keys() {
                out.push(TagEntry::NameValue(name.clone(), val));
            }
        }
    }
    out
}

/// Renders one side's target list as text fragments (the order matches
/// `forEachTarget`: TARGET_TYPES = wildcard, ip, subnet, tag, vm).
fn side_text(side: &Side) -> Vec<String> {
    let mut out = Vec::new();
    // wildcard
    for w in &side.wildcards {
        out.push(if w == "vmall" {
            "all vms".to_string()
        } else {
            w.clone()
        });
    }
    // ip
    for ip in &side.ips {
        out.push(format!("ip {ip}"));
    }
    // subnet
    for sn in &side.subnets {
        out.push(format!("subnet {sn}"));
    }
    // tag
    for tag in &side.tags {
        out.push(match tag {
            TagEntry::Name(n) => format!("tag {}", quote(n)),
            TagEntry::NameValue(n, v) => {
                format!("tag {} = {}", quote(n), quote(v))
            }
        });
    }
    // vm
    for vm in &side.vms {
        out.push(format!("vm {vm}"));
    }
    out
}

fn is_ipv4(s: &str) -> bool {
    s.parse::<std::net::Ipv4Addr>().is_ok()
}

fn is_ipv6(s: &str) -> bool {
    s.parse::<std::net::Ipv6Addr>().is_ok()
}

/// Generate a version string "<ms>.<pid6>" like node generateVersion().
pub fn generate_version() -> String {
    let ms = chrono::Utc::now().timestamp_millis();
    let pid = std::process::id();
    format!("{ms}.{pid:06}")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rule_from_text(s: &str) -> FwRule {
        FwRule::create(
            RuleData { rule: Some(s.to_string()), ..Default::default() },
            ParseOpts::default(),
        )
        .expect("create")
    }

    #[test]
    fn roundtrip_icmp6() {
        let r = rule_from_text("FROM any TO all vms ALLOW icmp6 TYPE all");
        assert_eq!(r.text(), "FROM any TO all vms ALLOW icmp6 TYPE all");
    }

    #[test]
    fn roundtrip_icmp4() {
        let r = rule_from_text("FROM any TO all vms ALLOW icmp TYPE all");
        assert_eq!(r.text(), "FROM any TO all vms ALLOW icmp TYPE all");
    }

    #[test]
    fn roundtrip_tcp_port() {
        let r = rule_from_text("FROM any TO all vms ALLOW tcp PORT 22");
        assert_eq!(r.text(), "FROM any TO all vms ALLOW tcp PORT 22");
    }

    #[test]
    fn roundtrip_multi_port() {
        let r = rule_from_text(
            "FROM any TO all vms ALLOW tcp (PORT 80 AND PORT 443)",
        );
        assert_eq!(
            r.text(),
            "FROM any TO all vms ALLOW tcp (PORT 80 AND PORT 443)"
        );
    }

    #[test]
    fn roundtrip_ports_range() {
        let r = rule_from_text(
            "FROM any TO all vms ALLOW udp PORTS 67, 68, 1024 - 2048",
        );
        assert_eq!(
            r.text(),
            "FROM any TO all vms ALLOW udp PORTS 67, 68, 1024 - 2048"
        );
    }

    #[test]
    fn roundtrip_tag_quoted() {
        let r = rule_from_text(
            "FROM tag \"role\" = \"db\" TO all vms ALLOW tcp PORT 5432",
        );
        assert_eq!(
            r.text(),
            "FROM tag \"role\" = \"db\" TO all vms ALLOW tcp PORT 5432"
        );
    }

    #[test]
    fn roundtrip_or_list_sorted() {
        // VMs dedup+sort; order normalizes.
        let r = rule_from_text(
            "FROM (vm 00000000-0000-0000-0000-000000000002 OR \
             vm 00000000-0000-0000-0000-000000000001) \
             TO all vms ALLOW tcp PORT 22",
        );
        assert_eq!(
            r.text(),
            "FROM (vm 00000000-0000-0000-0000-000000000001 OR \
             vm 00000000-0000-0000-0000-000000000002) \
             TO all vms ALLOW tcp PORT 22"
        );
    }

    #[test]
    fn roundtrip_priority() {
        let r = rule_from_text(
            "FROM any TO all vms BLOCK tcp PORT 25 PRIORITY 50",
        );
        assert_eq!(
            r.text(),
            "FROM any TO all vms BLOCK tcp PORT 25 PRIORITY 50"
        );
    }

    #[test]
    fn icmp_type_code_multi() {
        let r = rule_from_text(
            "FROM any TO all vms ALLOW icmp (TYPE 8 AND TYPE 0 CODE 0)",
        );
        // sorts by (type<<8)+code: type 0 (code 0) -> 0, type 8 -> 2048
        assert_eq!(
            r.text(),
            "FROM any TO all vms ALLOW icmp (TYPE 0 CODE 0 AND TYPE 8)"
        );
    }

    #[test]
    fn serialize_shape() {
        let r = FwRule::create(
            RuleData {
                rule: Some(
                    "FROM any TO all vms ALLOW icmp6 TYPE all".to_string(),
                ),
                uuid: Some(
                    "cb7e0661-af26-4392-a8b9-ff688beed8b3".to_string(),
                ),
                version: Some("1781894798231.003866".to_string()),
                enabled: Some(true),
                description: Some("allow all ICMPv6 types".to_string()),
                ..Default::default()
            },
            ParseOpts::default(),
        )
        .unwrap();
        let v = r.serialize();
        let s = serde_json::to_string(&v).unwrap();
        // global true (no owner), key order matches FIELDS.
        assert_eq!(
            s,
            "{\"description\":\"allow all ICMPv6 types\",\"enabled\":true,\
             \"global\":true,\"log\":false,\
             \"rule\":\"FROM any TO all vms ALLOW icmp6 TYPE all\",\
             \"uuid\":\"cb7e0661-af26-4392-a8b9-ff688beed8b3\",\
             \"version\":\"1781894798231.003866\"}"
        );
    }

    #[test]
    fn reject_no_vm_target() {
        let r = FwRule::create(
            RuleData {
                rule: Some(
                    "FROM ip 1.2.3.4 TO ip 5.6.7.8 ALLOW tcp PORT 22"
                        .to_string(),
                ),
                ..Default::default()
            },
            ParseOpts::default(),
        );
        assert!(r.is_err());
    }
}
