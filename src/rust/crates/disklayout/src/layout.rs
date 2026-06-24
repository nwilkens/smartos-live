// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.
//
// Copyright 2026 Edgecast Cloud LLC.

//! Faithful port of node_modules/disklayout.js layout computation.

use std::collections::BTreeMap;

use serde::Serialize;

// Minimum/maximum stripe widths for the raidz variants. raidz2/raidz3 may
// override to go narrower in some cases (see special cases below).
const Z1_MIN: i64 = 3;
const Z1_MAX: i64 = 6;
const Z2_MIN: i64 = 7;
const Z2_MAX: i64 = 12;
const Z3_MIN: i64 = 9;
const Z3_MAX: i64 = 20;

/// A disk as read from diskinfo. `size` stays a string to byte-for-byte match
/// node's JSON, which never coerces it to a number on output.
#[derive(Clone, Debug)]
pub struct Disk {
    pub disk_type: String,
    pub name: String,
    pub vid: String,
    pub pid: String,
    pub size: String,
    pub removable: bool,
    pub solid_state: bool,
}

impl Disk {
    fn size_num(&self) -> f64 {
        // node coerces the string via arithmetic (`size * 1`); a non-numeric
        // string would yield NaN there too, so parse-or-NaN matches.
        self.size.parse::<f64>().unwrap_or(f64::NAN)
    }
}

/// Output device: exactly the five fields node emits, in this order.
#[derive(Clone, Debug, Serialize)]
pub struct Device {
    pub name: String,
    pub vid: String,
    pub pid: String,
    pub size: String,
    pub solid_state: bool,
}

/// capacity is a string for `single` (node assigns the raw size string) and a
/// number for every other layout (node computes it arithmetically).
#[derive(Clone, Debug, Serialize)]
#[serde(untagged)]
pub enum Capacity {
    Str(String),
    Num(serde_json::Number),
}

#[derive(Clone, Debug, Serialize)]
pub struct Vdev {
    #[serde(rename = "type")]
    pub vdev_type: String,
    pub devices: Vec<Device>,
}

/// The computed configuration. Field order and conditional omission mirror the
/// JS object: spares?, vdevs, capacity, logs?, cache?. `single` emits raw disk
/// objects as vdevs rather than typed mirror/raidz vdevs, so vdevs is an enum.
#[derive(Clone, Debug, Serialize)]
pub struct Config {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub spares: Option<Vec<Device>>,
    pub vdevs: Vdevs,
    pub capacity: Capacity,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub logs: Option<Vec<Device>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cache: Option<Vec<Device>>,
}

#[derive(Clone, Debug, Serialize)]
#[serde(untagged)]
pub enum Vdevs {
    Single(Vec<Device>),
    Typed(Vec<Vdev>),
}

/// Error string identical to node's `config.error`.
#[derive(Debug)]
pub struct LayoutError(pub String);

fn num(v: f64) -> Capacity {
    // node integers below 2^53 serialize without a fractional part.
    match serde_json::Number::from_f64(v) {
        Some(n) => {
            if v.fract() == 0.0 && v.is_finite() {
                Capacity::Num(serde_json::Number::from(v as i64))
            } else {
                Capacity::Num(n)
            }
        }
        None => Capacity::Num(serde_json::Number::from(0)),
    }
}

/// Group device sizes into marketed capacity classes; returns rounded GB.
fn round_capacity(bytes: f64) -> f64 {
    let mb = bytes / 1_000_000.0;
    let thresholds: [f64; 6] = [500000.0, 150000.0, 80000.0, 20000.0, 4500.0, 1000.0];
    let mut bestfit = 0.0_f64;
    let mut bestdiff = 0.0_f64;

    for t in thresholds {
        let roundoff = ((mb + (t / 2.0).floor()) / t).floor() * t;
        let multiplier = (roundoff / t).powi(4);
        let diff = (mb - roundoff) * (mb - roundoff) * multiplier;

        if (mb - roundoff).abs() / mb > 0.05 {
            continue;
        }
        if diff < bestdiff || bestdiff == 0.0 {
            bestfit = roundoff;
            bestdiff = (mb - roundoff) * (mb - roundoff) * multiplier;
        }
    }

    if (bestfit - mb).abs() / mb < 0.05 {
        return bestfit / 1000.0;
    }
    (mb / 1000.0).round()
}

#[derive(Clone)]
struct Bucket {
    size: f64,
    solid_state: bool,
    disks: Vec<Disk>,
    smallest: f64,
    largest: f64,
}

/// Ordered map keyed by typespec, preserving node's Object key insertion order.
struct Inventory {
    keys: Vec<String>,
    map: BTreeMap<String, Bucket>,
}

impl Inventory {
    fn new() -> Self {
        Inventory { keys: Vec::new(), map: BTreeMap::new() }
    }
    fn order(&self) -> Vec<String> {
        // Only keys still present, in original insertion order.
        self.keys.iter().filter(|k| self.map.contains_key(*k)).cloned().collect()
    }
}

/// Merge buckets of the same solid_state and within 5% size of each other.
fn merge_types(inv: &mut Inventory) {
    let order = inv.order();
    for t0type in &order {
        for t1type in &order {
            if t0type == t1type {
                continue;
            }
            let (t0, t1) = match (inv.map.get(t0type), inv.map.get(t1type)) {
                (Some(a), Some(b)) => (a.clone(), b.clone()),
                _ => continue,
            };
            if t0.solid_state != t1.solid_state {
                continue;
            }
            if (t0.size - t1.size).abs() / t1.size > 0.05 {
                continue;
            }
            if t0.disks.len() > t1.disks.len() {
                let mut merged = t0.disks.clone();
                merged.extend(t1.disks.clone());
                if let Some(b) = inv.map.get_mut(t0type) {
                    b.disks = merged;
                }
                inv.map.remove(t1type);
            } else {
                let mut merged = t1.disks.clone();
                merged.extend(t0.disks.clone());
                if let Some(b) = inv.map.get_mut(t1type) {
                    b.disks = merged;
                }
                inv.map.remove(t0type);
            }
        }
    }
}

fn shrink(inv: &mut Inventory) {
    for key in inv.order() {
        if let Some(t) = inv.map.get_mut(&key) {
            let mut smallest = 0.0_f64;
            let mut largest = 0.0_f64;
            for d in &t.disks {
                let s = d.size_num();
                if smallest == 0.0 || s < smallest {
                    smallest = s;
                }
                if largest == 0.0 || s > largest {
                    largest = s;
                }
            }
            t.smallest = smallest;
            t.largest = largest;
        }
    }
}

/// Reduce a bucket to its sorted (size asc, then name asc) device list.
fn xform_bucket(bucket: &Bucket) -> Vec<Device> {
    let mut role: Vec<Device> = bucket
        .disks
        .iter()
        .map(|d| Device {
            name: d.name.clone(),
            vid: d.vid.clone(),
            pid: d.pid.clone(),
            size: d.size.clone(),
            solid_state: d.solid_state,
        })
        .collect();
    role.sort_by(|a, b| {
        let an = a.size.parse::<f64>().unwrap_or(f64::NAN);
        let bn = b.size.parse::<f64>().unwrap_or(f64::NAN);
        if an != bn {
            an.partial_cmp(&bn).unwrap_or(std::cmp::Ordering::Equal)
        } else {
            a.name.cmp(&b.name)
        }
    });
    role
}

#[derive(Default)]
struct Roles {
    storage: Option<Vec<Device>>,
    cache: Option<Vec<Device>>,
    slog: Option<Vec<Device>>,
}

/// Assign storage/cache/slog roles to the size buckets, per the JS heuristics.
fn assign_roles(inv: &Inventory, enable_cache: bool) -> Roles {
    let mut roles = Roles::default();
    let typedescs = inv.order();
    if typedescs.is_empty() {
        return roles;
    }
    if typedescs.len() == 1 {
        if let Some(b) = inv.map.get(&typedescs[0]) {
            roles.storage = Some(xform_bucket(b));
        }
        return roles;
    }

    let sortkey = |d: &String| inv.map.get(d).map(|b| b.smallest).unwrap_or(0.0);

    let mut ssddescs: Vec<String> =
        typedescs.iter().filter(|d| inv.map.get(*d).map(|b| b.solid_state).unwrap_or(false)).cloned().collect();
    ssddescs.sort_by(|a, b| sortkey(a).partial_cmp(&sortkey(b)).unwrap_or(std::cmp::Ordering::Equal));

    let mut rustdescs: Vec<String> =
        typedescs.iter().filter(|d| !inv.map.get(*d).map(|b| b.solid_state).unwrap_or(false)).cloned().collect();
    rustdescs.sort_by(|a, b| sortkey(a).partial_cmp(&sortkey(b)).unwrap_or(std::cmp::Ordering::Equal));

    if !rustdescs.is_empty() {
        if let Some(largest) = rustdescs.pop()
            && let Some(b) = inv.map.get(&largest)
        {
            roles.storage = Some(xform_bucket(b));
        }
    }

    if enable_cache {
        for typedesc in &rustdescs {
            if let Some(b) = inv.map.get(typedesc) {
                let role = xform_bucket(b);
                match roles.cache {
                    Some(ref mut c) => c.extend(role),
                    None => roles.cache = Some(role),
                }
            }
        }
    }

    if ssddescs.is_empty() {
        return roles;
    }

    if roles.storage.is_none()
        && let Some(largest) = ssddescs.pop()
        && let Some(b) = inv.map.get(&largest)
    {
        roles.storage = Some(xform_bucket(b));
    }

    if ssddescs.is_empty() {
        return roles;
    }

    let smallest_ssd_len = inv.map.get(&ssddescs[0]).map(|b| b.disks.len()).unwrap_or(0);
    if smallest_ssd_len < 5 {
        if let Some(b) = inv.map.get(&ssddescs[0]) {
            roles.slog = Some(xform_bucket(b));
        }
        ssddescs.remove(0);
    }

    if enable_cache {
        for typedesc in &ssddescs {
            if let Some(b) = inv.map.get(typedesc) {
                let role = xform_bucket(b);
                match roles.cache {
                    Some(ref mut c) => c.extend(role),
                    None => roles.cache = Some(role),
                }
            }
        }
    }

    roles
}

struct PartialConfig {
    spares: Option<Vec<Device>>,
    vdevs: Vdevs,
    capacity: Capacity,
}

fn do_single(disks: &[Device], nspares: Option<i64>) -> Result<PartialConfig, LayoutError> {
    if let Some(n) = nspares
        && n != 0
    {
        return Err(LayoutError("spares are not allowed for a single disk pool".into()));
    }
    let first = disks[0].clone();
    let cap = Capacity::Str(first.size.clone());
    Ok(PartialConfig { spares: None, vdevs: Vdevs::Single(vec![first]), capacity: cap })
}

fn do_mirror(disks: &mut Vec<Device>, nspares: Option<i64>, width: Option<i64>) -> Result<PartialConfig, LayoutError> {
    if disks.len() < 2 {
        return Err(LayoutError("at least 2 disks are required for mirroring".into()));
    }

    let mut width = width;
    let mut fixedwidth = false;
    let total = disks.len() as i64;

    let w = match width {
        None => 2,
        Some(w) => {
            let mut dlen = total;
            if let Some(ns) = nspares {
                if ns >= total {
                    return Err(LayoutError(format!("{} is an invalid number of spares for {} disks", ns, total)));
                }
                dlen -= ns;
            }
            if w < 2 || w > dlen {
                return Err(LayoutError(format!("{} is an invalid width for mirroring and {} disks", w, dlen)));
            }
            fixedwidth = true;
            w
        }
    };
    width = Some(w);
    let w = w;

    let spares: i64 = match nspares {
        None => {
            if fixedwidth {
                total % w
            } else if total == 2 || total == 4 {
                0
            } else {
                let mut s = (total as f64 / 16.0).ceil() as i64;
                s += (total - s) % w;
                s
            }
        }
        Some(ns) => {
            if ns > (total - w) {
                return Err(LayoutError(format!("{} is an invalid number of spares for {} disks", ns, total)));
            }
            ns
        }
    };

    let mut out_spares = None;
    if spares > 0 {
        let at = disks.len() - spares as usize;
        out_spares = Some(disks.split_off(at));
    }

    let mut vdevs: Vec<Vdev> = Vec::new();
    let mut capacity = 0.0_f64;
    while !disks.is_empty() {
        capacity += disks[0].size.parse::<f64>().unwrap_or(f64::NAN);
        let take = std::cmp::min(w as usize, disks.len());
        let devices: Vec<Device> = disks.drain(0..take).collect();
        vdevs.push(Vdev { vdev_type: "mirror".into(), devices });
        if fixedwidth && (disks.len() as i64) < width.unwrap_or(w) {
            break;
        }
    }

    Ok(PartialConfig { spares: out_spares, vdevs: Vdevs::Typed(vdevs), capacity: num(capacity) })
}

#[allow(clippy::too_many_arguments)]
fn raidz_common(
    disks: &mut Vec<Device>,
    rtype: &str,
    minwidth: i64,
    maxwidth: i64,
    mut spares: i64,
    fixedspares: bool,
    width: Option<i64>,
) -> Result<PartialConfig, LayoutError> {
    let total = disks.len() as i64;
    let mut fixedwidth = false;
    let chosen_width: i64;

    match width {
        Some(w) => {
            if w < minwidth || w > total {
                return Err(LayoutError(format!(
                    "{} is an invalid width for {} and {} disks (min: {}, default max: {})",
                    w, rtype, total, minwidth, maxwidth
                )));
            }
            fixedwidth = true;
            if !fixedspares {
                spares = total % w;
            } else if total - spares < w {
                return Err(LayoutError(format!("{} is an invalid width for {} and {} spares", w, rtype, spares)));
            }
            chosen_width = w;
        }
        None => {
            let mut w = minwidth;
            loop {
                if total - spares < minwidth {
                    break;
                }
                w = minwidth;
                while w <= maxwidth {
                    if (total - spares) % w == 0 {
                        break;
                    }
                    w += 1;
                }
                if w <= maxwidth {
                    break;
                }
                if fixedspares {
                    return Err(LayoutError(format!(
                        "no acceptable {} layout is possible with {} disks and {} {}",
                        rtype,
                        total,
                        spares,
                        if spares == 1 { "spare" } else { "spares" }
                    )));
                }
                spares += 1;
            }
            chosen_width = w;
        }
    }

    if total - spares < minwidth {
        return Err(LayoutError(format!("no acceptable {} layout is possible with {} disks", rtype, total)));
    }

    let mut out_spares = None;
    if spares > 0 {
        let at = disks.len() - spares as usize;
        out_spares = Some(disks.split_off(at));
    }

    let nparity: f64 = match rtype {
        "raidz" => 1.0,
        "raidz2" => 2.0,
        "raidz3" => 3.0,
        _ => return Err(LayoutError(format!("invalid raid type: {}", rtype))),
    };

    let w = chosen_width;
    let mut vdevs: Vec<Vdev> = Vec::new();
    let mut capacity = 0.0_f64;
    while !disks.is_empty() {
        capacity += (w as f64 - nparity) * disks[0].size.parse::<f64>().unwrap_or(f64::NAN);
        let take = std::cmp::min(w as usize, disks.len());
        let devices: Vec<Device> = disks.drain(0..take).collect();
        vdevs.push(Vdev { vdev_type: rtype.into(), devices });
        if fixedwidth && (disks.len() as i64) < w {
            break;
        }
    }

    Ok(PartialConfig { spares: out_spares, vdevs: Vdevs::Typed(vdevs), capacity: num(capacity) })
}

fn do_raidz1(disks: &mut Vec<Device>, nspares: Option<i64>, width: Option<i64>) -> Result<PartialConfig, LayoutError> {
    let (spares, fixedspares) = match nspares {
        None => (std::cmp::min(2, (disks.len() as i64) / 7), false),
        Some(ns) => (ns, true),
    };
    raidz_common(disks, "raidz", Z1_MIN, Z1_MAX, spares, fixedspares, width)
}

fn do_raidz2(disks: &mut Vec<Device>, nspares: Option<i64>, width: Option<i64>) -> Result<PartialConfig, LayoutError> {
    let mut minwidth = Z2_MIN;
    let mut maxwidth = Z2_MAX;
    let len = disks.len() as i64;
    if len == 5 || len == 6 {
        minwidth = len;
        maxwidth = len;
    }
    let (spares, fixedspares) = match nspares {
        None => (std::cmp::min(2, len / Z2_MAX), false),
        Some(ns) => (ns, true),
    };
    raidz_common(disks, "raidz2", minwidth, maxwidth, spares, fixedspares, width)
}

fn do_raidz3(disks: &mut Vec<Device>, nspares: Option<i64>, width: Option<i64>) -> Result<PartialConfig, LayoutError> {
    let mut minwidth = Z3_MIN;
    let mut maxwidth = Z3_MAX;
    let len = disks.len() as i64;
    if len == 7 || len == 8 {
        minwidth = len;
        maxwidth = len;
    }
    let (spares, fixedspares) = match nspares {
        None => (std::cmp::min(2, len / 15), false),
        Some(ns) => (ns, true),
    };
    raidz_common(disks, "raidz3", minwidth, maxwidth, spares, fixedspares, width)
}

pub fn list_supported() -> Vec<&'static str> {
    vec!["single", "mirror", "raidz1", "raidz2", "raidz3"]
}

fn is_valid(storage: &[Device], nspares: Option<i64>) -> Result<(), LayoutError> {
    const PRIMES: [i64; 13] = [17, 19, 23, 29, 31, 37, 41, 43, 47, 53, 59, 61, 67];
    if let Some(ns) = nspares {
        let ndisks = storage.len() as i64 - ns;
        if ns > 0 && ndisks < 2 {
            return Err(LayoutError(format!("too many spares ({}) for this disk configuration", ns)));
        }
        for p in PRIMES {
            if ndisks == p {
                return Err(LayoutError(format!("invalid number of spares ({}) for this disk configuration", ns)));
            }
        }
    }
    Ok(())
}

/// Auto-select a layout for disk counts not covered by the simple switch.
fn complex_layout(storage: &[Device], navail: i64, nspares: Option<i64>, width: Option<i64>) -> &'static str {
    if let Some(w) = width {
        if w <= 2 {
            return "mirror";
        }
        if w <= Z1_MAX {
            return "raidz1";
        }
        if w <= Z2_MAX {
            return "raidz2";
        }
        return "raidz3";
    }

    let first_size = storage.first().map(|d| d.size.parse::<f64>().unwrap_or(f64::NAN)).unwrap_or(f64::NAN);

    if navail > 16 || (navail > Z2_MIN && first_size > 1500.0 * 1_000_000_000.0) {
        if nspares.is_none() {
            return "raidz2";
        }
        for i in Z2_MIN..=Z2_MAX {
            if navail % i == 0 {
                return "raidz2";
            }
        }
        if navail == 13 {
            return "raidz3";
        }
        return "raidz1";
    }

    if navail < Z2_MAX && storage.first().map(|d| d.solid_state).unwrap_or(false) {
        if navail > Z1_MAX {
            return "raidz2";
        }
        return "raidz1";
    }

    if navail % 2 != 0 {
        if nspares.is_none() {
            return "mirror";
        }
        if navail < 6 {
            return "raidz1";
        }
        if navail == 15 {
            return "raidz1";
        }
        if navail == 13 {
            return "raidz3";
        }
        return "raidz2";
    }

    "mirror"
}

/// Compute a pool configuration. Mirrors compute_layout in disklayout.js.
pub fn compute(
    disks: Vec<Disk>,
    layout: Option<String>,
    nspares: Option<i64>,
    enable_cache: bool,
    width: Option<i64>,
) -> Result<Config, LayoutError> {
    let mut inv = Inventory::new();

    for disk in disks {
        if disk.removable {
            continue;
        }
        let gb = round_capacity(disk.size_num());
        if gb == 0.0 {
            continue;
        }
        // Match node's float-to-string in the typespec key (e.g. "1000" or "0.5").
        let typespec = format!("{},{},{}", disk.disk_type, fmt_num(gb), disk.solid_state);
        if !inv.map.contains_key(&typespec) {
            inv.keys.push(typespec.clone());
            inv.map.insert(
                typespec.clone(),
                Bucket {
                    size: gb,
                    solid_state: disk.solid_state,
                    disks: Vec::new(),
                    smallest: 0.0,
                    largest: 0.0,
                },
            );
        }
        if let Some(b) = inv.map.get_mut(&typespec) {
            b.disks.push(disk);
        }
    }

    merge_types(&mut inv);
    shrink(&mut inv);
    let roles = assign_roles(&inv, enable_cache);

    let storage = match roles.storage {
        Some(s) => s,
        None => return Err(LayoutError("no primary storage disks available".into())),
    };

    is_valid(&storage, nspares)?;

    let layout = match layout {
        Some(l) if !l.is_empty() => l,
        _ => {
            let navail = match nspares {
                None => storage.len() as i64,
                Some(ns) => storage.len() as i64 - ns,
            };
            let chosen = match navail {
                1 => "single",
                2 => "mirror",
                3 | 4 => "raidz1",
                5 | 6 => {
                    if storage.first().map(|d| d.solid_state).unwrap_or(false) || nspares.is_some() {
                        "raidz1"
                    } else {
                        "raidz2"
                    }
                }
                _ => complex_layout(&storage, navail, nspares, width),
            };
            chosen.to_string()
        }
    };

    let mut work = storage;
    let part = match layout.as_str() {
        "single" => do_single(&work, nspares)?,
        "mirror" => do_mirror(&mut work, nspares, width)?,
        "raidz1" => do_raidz1(&mut work, nspares, width)?,
        "raidz2" => do_raidz2(&mut work, nspares, width)?,
        "raidz3" => do_raidz3(&mut work, nspares, width)?,
        other => return Err(LayoutError(format!("unknown layout {}", other))),
    };

    Ok(Config {
        spares: part.spares,
        vdevs: part.vdevs,
        capacity: part.capacity,
        logs: roles.slog,
        cache: roles.cache,
    })
}

/// Format a float the way JS string-coerces a number for object keys: integers
/// without a decimal point, otherwise the shortest round-tripping form.
fn fmt_num(v: f64) -> String {
    if v.fract() == 0.0 && v.is_finite() {
        format!("{}", v as i64)
    } else {
        let s = format!("{}", v);
        s
    }
}
