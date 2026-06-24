// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.
//
// Copyright 2026 Edgecast Cloud LLC.

//! Disklayout JSON parsing and `zpool create` argv construction.
//!
//! Mirrors node `zfs.zpool.create()` (src/node_modules/zfs.js) exactly so the
//! command built here is byte-for-byte identical to the legacy tool.

use serde::Deserialize;

// Paths from zfs.js `exports.paths`.
pub const ZPOOL: &str = "/sbin/zpool";
pub const KBMADM: &str = "/usr/sbin/kbmadm";

#[derive(Debug, Deserialize)]
pub struct Device {
    pub name: String,
}

#[derive(Debug, Deserialize)]
pub struct Vdev {
    /// Grouping keyword ("mirror", "raidz", ...). Absent for a bare device.
    #[serde(default)]
    pub r#type: Option<String>,
    /// Member devices when this vdev is a group.
    #[serde(default)]
    pub devices: Option<Vec<Device>>,
    /// Device name when this vdev is a bare disk (no `devices`).
    #[serde(default)]
    pub name: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct Config {
    #[serde(default)]
    pub vdevs: Vec<Vdev>,
    #[serde(default)]
    pub spares: Option<Vec<Device>>,
    #[serde(default)]
    pub logs: Option<Vec<Device>>,
    #[serde(default)]
    pub cache: Option<Vec<Device>>,
}

/// Result of constructing the create invocation: program plus argv.
pub struct Command {
    pub program: String,
    pub args: Vec<String>,
}

/// Build the `zpool create` (or `kbmadm create-zpool`) command for `pool`.
///
/// `force`/`encrypt`/`efi` map to mkzpool's `-f`/`-e`/`-B`. The ordering and
/// grouping here intentionally match zfs.js line-for-line.
pub fn build_command(pool: &str, config: &Config, force: bool, encrypt: bool, efi: bool) -> Command {
    let (program, mut args): (String, Vec<String>) = if encrypt {
        (KBMADM.to_string(), vec!["create-zpool".into(), "--".into()])
    } else {
        (ZPOOL.to_string(), vec!["create".into()])
    };

    if efi {
        args.push("-B".into());
    }
    if force {
        args.push("-f".into());
    }

    args.push(pool.to_string());

    for vdev in &config.vdevs {
        if let Some(t) = &vdev.r#type {
            args.push(t.clone());
        }
        if let Some(devices) = &vdev.devices {
            for dev in devices {
                args.push(dev.name.clone());
            }
        } else if let Some(name) = &vdev.name {
            args.push(name.clone());
        }
    }

    if let Some(spares) = &config.spares {
        args.push("spare".into());
        for dev in spares {
            args.push(dev.name.clone());
        }
    }

    if let Some(logs) = &config.logs {
        args.push("log".into());
        for dev in logs {
            args.push(dev.name.clone());
        }
    }

    if let Some(cache) = &config.cache {
        args.push("cache".into());
        for dev in cache {
            args.push(dev.name.clone());
        }
    }

    Command { program, args }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(s: &str) -> Config {
        serde_json::from_str(s).expect("valid json")
    }

    #[test]
    fn single_bare_disk() {
        let cfg = parse(
            r#"{"vdevs":[{"name":"disk0","size":"1"}],"capacity":"1"}"#,
        );
        let c = build_command("zones", &cfg, true, false, true);
        assert_eq!(c.program, ZPOOL);
        assert_eq!(c.args, vec!["create", "-B", "-f", "zones", "disk0"]);
    }

    #[test]
    fn mirror_group() {
        let cfg = parse(
            r#"{"vdevs":[{"type":"mirror","devices":[{"name":"disk0"},{"name":"disk1"}]}]}"#,
        );
        let c = build_command("zones", &cfg, true, false, true);
        assert_eq!(
            c.args,
            vec!["create", "-B", "-f", "zones", "mirror", "disk0", "disk1"]
        );
    }

    #[test]
    fn no_flags() {
        let cfg = parse(r#"{"vdevs":[{"name":"disk0"}]}"#);
        let c = build_command("zones", &cfg, false, false, false);
        assert_eq!(c.args, vec!["create", "zones", "disk0"]);
    }

    #[test]
    fn spares_logs_cache_order() {
        let cfg = parse(
            r#"{
              "vdevs":[{"type":"mirror","devices":[{"name":"disk0"},{"name":"disk1"}]}],
              "spares":[{"name":"disk2"}],
              "logs":[{"name":"disk3"}],
              "cache":[{"name":"disk4"}]
            }"#,
        );
        let c = build_command("zones", &cfg, true, false, true);
        assert_eq!(
            c.args,
            vec![
                "create", "-B", "-f", "zones", "mirror", "disk0", "disk1", "spare", "disk2", "log",
                "disk3", "cache", "disk4"
            ]
        );
    }

    #[test]
    fn encrypt_uses_kbmadm() {
        let cfg = parse(r#"{"vdevs":[{"name":"disk0"}]}"#);
        let c = build_command("zones", &cfg, true, true, true);
        assert_eq!(c.program, KBMADM);
        assert_eq!(
            c.args,
            vec!["create-zpool", "--", "-B", "-f", "zones", "disk0"]
        );
    }
}
