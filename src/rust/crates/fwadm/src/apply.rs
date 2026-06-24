// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.
//
// Copyright 2026 Edgecast Cloud LLC.

//! ipf apply layer. Ports `src/fw/lib/ipf.js`: writes the per-zone
//! ipf.conf/ipf6.conf with the atomic-swap (replaceIPFconf) and invokes
//! ipf(8)/ipfstat(8) (zoneReload/start/stop/status/stats).

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::error::FwError;
use crate::ipf::IpfConf;

const IPF: &str = "/usr/sbin/ipf";
const IPFSTAT: &str = "/usr/sbin/ipfstat";

/// Whether the platform supports the `-G` flag (newer platforms do).
#[derive(Debug, Clone, Copy, Default)]
pub struct IpfCtl {
    /// True on older platforms lacking `-G`.
    pub old: bool,
}

fn conf_paths(zonepath: &str) -> (PathBuf, PathBuf) {
    (
        Path::new(zonepath).join("config/ipf.conf"),
        Path::new(zonepath).join("config/ipf6.conf"),
    )
}

/// replaceIPFconf: write temp -> remove .old -> hardlink current to .old ->
/// rename temp over current.
fn replace_ipf_conf(path: &Path, contents: &str) -> Result<(), FwError> {
    let ver =
        format!("{}", chrono::Utc::now().timestamp_millis());
    let temp = path.with_extension(format!("conf.{ver}"));
    let old = path.with_extension("conf.old");

    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }

    let mut f = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o644)
        .open(&temp)?;
    f.write_all(contents.as_bytes())?;
    drop(f);

    // Remove the stale .old, then hardlink the current to .old if it exists.
    let _ = fs::remove_file(&old);
    if path.exists() {
        let _ = fs::hard_link(path, &old);
    }
    fs::rename(&temp, path)?;
    Ok(())
}

/// Write both conf files for a zone.
pub fn write_confs(conf: &IpfConf) -> Result<(), FwError> {
    let (v4, v6) = conf_paths(&conf.zonepath);
    replace_ipf_conf(&v4, &conf.v4text)?;
    replace_ipf_conf(&v6, &conf.v6text)?;
    Ok(())
}

/// zoneReload: single ipf(8) invocation loading both conf files and swapping.
pub fn reload(
    ctl: IpfCtl,
    uuid: &str,
    conf4: &Path,
    conf6: &Path,
) -> Result<(), FwError> {
    let conf4 = conf4.to_string_lossy().to_string();
    let conf6 = conf6.to_string_lossy().to_string();
    let mut args: Vec<String> = vec![
        "-G".into(),
        "-E".into(),
        "-I".into(),
        "-Fa".into(),
        "-f".into(),
        conf4,
        "-6".into(),
        "-Fa".into(),
        "-f".into(),
        conf6,
        "-sy".into(),
        uuid.into(),
    ];
    if ctl.old {
        args.remove(0);
    }
    run(IPF, &args)
}

/// zoneStart: ipf -GE <uuid>.
pub fn start(ctl: IpfCtl, uuid: &str) -> Result<(), FwError> {
    let args: Vec<String> = if ctl.old {
        vec!["-E".into(), uuid.into()]
    } else {
        vec!["-GE".into(), uuid.into()]
    };
    run(IPF, &args)
}

/// zoneStop: ipf -GD <uuid>.
pub fn stop(ctl: IpfCtl, uuid: &str) -> Result<(), FwError> {
    let args: Vec<String> = if ctl.old {
        vec!["-D".into(), uuid.into()]
    } else {
        vec!["-GD".into(), uuid.into()]
    };
    run(IPF, &args)
}

/// zoneStatus: ipf -GV <uuid>, parsed into key/value pairs.
pub fn status(
    ctl: IpfCtl,
    uuid: &str,
) -> Result<Vec<(String, StatusVal)>, FwError> {
    let args: Vec<String> = if ctl.old {
        vec!["-V".into(), uuid.into()]
    } else {
        vec!["-GV".into(), uuid.into()]
    };
    let out = output(IPF, &args)?;
    if out.is_empty() {
        return Err(FwError::generic("No output from ipf"));
    }
    let mut results = Vec::new();
    for line in out.lines() {
        let Some(idx) = line.find(':') else { continue };
        let key = line[..idx].to_lowercase();
        let mut val = line[idx + 1..].trim().to_string();
        if key.is_empty() {
            continue;
        }
        let parsed = match key.as_str() {
            "ipf" | "kernel" => {
                val = val.replace("IP Filter: ", "");
                StatusVal::Str(val)
            }
            "running" => StatusVal::Bool(val != "no"),
            "active list" => {
                StatusVal::Num(val.parse().unwrap_or(0))
            }
            _ => StatusVal::Str(val),
        };
        results.push((key, parsed));
    }
    Ok(results)
}

/// A parsed status value.
#[derive(Debug, Clone)]
pub enum StatusVal {
    /// A string value.
    Str(String),
    /// A boolean (running).
    Bool(bool),
    /// A number (active list).
    Num(i64),
}

/// zoneRuleStats: ipfstat -hoi -G <uuid>, returns (hits, rule) pairs.
pub fn rule_stats(
    ctl: IpfCtl,
    uuid: &str,
) -> Result<Vec<(String, String)>, FwError> {
    let args: Vec<String> = if ctl.old {
        vec!["-hoi".into(), "-z".into(), uuid.into()]
    } else {
        vec!["-hoi".into(), "-G".into(), uuid.into()]
    };
    let out = output(IPFSTAT, &args)?;
    if out.is_empty() {
        return Err(FwError::generic("No output from ipfstat"));
    }
    let mut results = Vec::new();
    for line in out.lines() {
        if line.is_empty() {
            continue;
        }
        let Some(idx) = line.find(' ') else { continue };
        results.push((line[..idx].to_string(), line[idx + 1..].to_string()));
    }
    Ok(results)
}

fn run(cmd: &str, args: &[String]) -> Result<(), FwError> {
    let status = Command::new(cmd).args(args).status().map_err(|e| {
        FwError::generic(format!("failed to run {cmd}: {e}"))
    })?;
    if status.success() {
        Ok(())
    } else {
        Err(FwError::generic(format!(
            "{cmd} exited with {}",
            status.code().unwrap_or(-1)
        )))
    }
}

fn output(cmd: &str, args: &[String]) -> Result<String, FwError> {
    let out = Command::new(cmd).args(args).output().map_err(|e| {
        FwError::generic(format!("failed to run {cmd}: {e}"))
    })?;
    Ok(String::from_utf8_lossy(&out.stdout).to_string())
}
