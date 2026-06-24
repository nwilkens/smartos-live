// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.
//
// Copyright 2026 Edgecast Cloud LLC.

//! ZFS / zoneadm interaction: installed-image enumeration and the
//! `zfs receive` install pipeline.
//!
//! Consistent with the rest of this workspace (e.g. `mkzpool`), zfs
//! and zoneadm are driven via `std::process::Command`; there is no
//! vendored illumos/zfs Rust crate to reuse here.

use std::collections::HashMap;
use std::io::{Read, Write};
use std::process::{Command, Stdio};

use imgapi_manifest::Compression;

use crate::error::{Error, Result};

const ZFS: &str = "/usr/sbin/zfs";
const ZONEADM: &str = "/usr/sbin/zoneadm";

/// An enumerated installed image (before db manifest merge).
pub struct InstalledImage {
    pub zpool: String,
    pub uuid: String,
    pub clone_names: Vec<String>,
}

/// Enumerate installed images by intersecting `zoneadm list -pc`
/// with `zfs list`. Mirrors node `_loadImages`.
pub fn load_images() -> Result<Vec<InstalledImage>> {
    let zone_roots = zone_roots()?;

    let out = run(
        ZFS,
        &[
            "list",
            "-t",
            "filesystem,volume,snapshot",
            "-pH",
            "-o",
            "name,origin,mountpoint,imgadm:ignore",
        ],
    )?;

    // First pass: which datasets have an `@final` snapshot.
    let mut has_final: HashMap<String, bool> = HashMap::new();
    for line in out.lines() {
        let name = line.split('\t').next().unwrap_or("");
        if let Some(base) = name.strip_suffix("@final") {
            has_final.insert(base.to_string(), true);
        }
    }

    let mut image_names: Vec<String> = Vec::new();
    let mut usage: HashMap<String, Vec<String>> = HashMap::new();

    for line in out.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let parts: Vec<&str> = line.split('\t').collect();
        if parts.len() != 4 {
            continue;
        }
        let name = parts[0];
        let origin = parts[1];
        let mountpoint = parts[2];
        let ignore = parts[3];

        if !fs_name_re(name) {
            continue;
        }

        let is_image = !zone_roots.contains(mountpoint)
            && img_name_re(name)
            && (!origin.ends_with("@final") || has_final.contains_key(name));

        if is_image {
            let ignored = bool_from_zfs_prop(ignore);
            if !ignored {
                image_names.push(name.to_string());
            }
        }

        if origin != "-" {
            let oname = origin.split('@').next().unwrap_or("").to_string();
            usage.entry(oname).or_default().push(name.to_string());
        }
    }

    let mut images = Vec::with_capacity(image_names.len());
    for name in image_names {
        let Some((zpool, uuid)) = split_fs_name(&name) else {
            continue;
        };
        let clone_names = usage.get(&name).cloned().unwrap_or_default();
        images.push(InstalledImage {
            zpool,
            uuid,
            clone_names,
        });
    }
    Ok(images)
}

fn zone_roots() -> Result<std::collections::HashSet<String>> {
    let out = run(ZONEADM, &["list", "-pc"])?;
    let mut roots = std::collections::HashSet::new();
    for line in out.lines() {
        // Fields: id:name:state:zoneroot:uuid:brand:ip-type:...
        if let Some(root) = line.split(':').nth(3) {
            roots.insert(root.to_string());
        }
    }
    Ok(roots)
}

/// node `boolFromString(ignore, false)` with `-` => false.
fn bool_from_zfs_prop(v: &str) -> bool {
    match v {
        "-" => false,
        "true" | "1" | "on" | "yes" => true,
        _ => false,
    }
}

/// VMADM_FS_NAME_RE: `<pool>/<uuid>` optionally `[-/]diskN`.
/// The uuid is exactly 36 chars, so we validate that prefix and any
/// disk suffix directly (a plain `-` split would break on uuid hyphens).
fn fs_name_re(name: &str) -> bool {
    let Some((pool, rest)) = name.split_once('/') else {
        return false;
    };
    if !valid_pool(pool) {
        return false;
    }
    if is_uuid(rest) {
        return true;
    }
    if rest.len() > 36 && is_uuid(&rest[..36]) {
        return disk_suffix(&rest[36..]);
    }
    false
}

fn disk_suffix(s: &str) -> bool {
    // matches ([-/]disk\d+)?
    if s.is_empty() {
        return true;
    }
    let Some(rest) = s.strip_prefix('-').or_else(|| s.strip_prefix('/')) else {
        return false;
    };
    let Some(num) = rest.strip_prefix("disk") else {
        return false;
    };
    !num.is_empty() && num.chars().all(|c| c.is_ascii_digit())
}

/// VMADM_IMG_NAME_RE: exactly `<pool>/<uuid>` (no disk suffix).
fn img_name_re(name: &str) -> bool {
    match name.split_once('/') {
        Some((pool, rest)) => valid_pool(pool) && is_uuid(rest),
        None => false,
    }
}

fn valid_pool(p: &str) -> bool {
    let mut chars = p.chars();
    match chars.next() {
        Some(c) if c.is_ascii_alphabetic() => {}
        _ => return false,
    }
    p.chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
}

pub fn is_uuid(s: &str) -> bool {
    let b = s.as_bytes();
    if b.len() != 36 {
        return false;
    }
    for (i, &c) in b.iter().enumerate() {
        match i {
            8 | 13 | 18 | 23 => {
                if c != b'-' {
                    return false;
                }
            }
            _ => {
                if !c.is_ascii_hexdigit() || c.is_ascii_uppercase() {
                    return false;
                }
            }
        }
    }
    true
}

fn split_fs_name(name: &str) -> Option<(String, String)> {
    let (pool, rest) = name.split_once('/')?;
    let uuid = if is_uuid(rest) {
        rest
    } else if rest.len() > 36 && is_uuid(&rest[..36]) {
        &rest[..36]
    } else {
        return None;
    };
    Some((pool.to_string(), uuid.to_string()))
}

/// Whether the dataset `<zpool>/<uuid>` exists.
pub fn dataset_exists(zpool: &str, uuid: &str) -> Result<bool> {
    let name = format!("{zpool}/{uuid}");
    let status = Command::new(ZFS)
        .args(["list", "-H", "-o", "name", &name])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()?;
    Ok(status.success())
}

/// Children of a dataset: child snapshots and dependent clones.
/// Returns `{"snapshots": [...], "clones": [...]}` matching the node
/// `children` object on `get -r`.
pub fn dataset_children(zpool: &str, uuid: &str) -> Result<serde_json::Value> {
    let name = format!("{zpool}/{uuid}");
    // Snapshots of this dataset.
    let snap_out = run(ZFS, &["list", "-t", "all", "-pHr", "-o", "name", &name])?;
    let snapshots: Vec<String> = snap_out
        .lines()
        .map(str::trim)
        .filter(|l| l.starts_with(&format!("{name}@")))
        .map(str::to_string)
        .collect();

    // Dependent clones: datasets whose origin is one of our snapshots.
    let clone_out = run(
        ZFS,
        &["list", "-t", "filesystem,volume", "-o", "origin,name", "-pH"],
    )?;
    let mut clones = Vec::new();
    for line in clone_out.lines() {
        let mut it = line.split('\t');
        let origin = it.next().unwrap_or("");
        let cname = it.next().unwrap_or("");
        if origin.split('@').next() == Some(&name) {
            clones.push(cname.to_string());
        }
    }

    Ok(serde_json::json!({
        "snapshots": snapshots,
        "clones": clones,
    }))
}

/// Destroy `<zpool>/<uuid>` recursively.
pub fn destroy(zpool: &str, uuid: &str) -> Result<()> {
    let name = format!("{zpool}/{uuid}");
    run(ZFS, &["destroy", "-r", &name])?;
    Ok(())
}

/// Best-effort destroy used for rollback; never errors.
pub fn destroy_quiet(name: &str) {
    let _ = Command::new(ZFS)
        .args(["destroy", "-r", name])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
}

/// Receive an image stream into `<zpool>/<uuid>-partial`, ensure a
/// single `@final` snapshot, then rename to `<zpool>/<uuid>`.
///
/// `file_path` is the downloaded (still-compressed) image file. The
/// gunzip step is done in-process with flate2; bzip2/xz are shelled
/// to the platform binaries (matching node and avoiding extra deps).
/// On any error the partial dataset is destroyed.
pub fn install_zfs_image(
    file_path: &std::path::Path,
    zpool: &str,
    uuid: &str,
    compression: Compression,
    zstream: bool,
) -> Result<()> {
    let ds_name = format!("{zpool}/{uuid}");
    let partial = format!("{ds_name}-partial");

    let result = recv_pipeline(file_path, &partial, compression, zstream)
        .and_then(|()| ensure_final_snapshot(uuid, &partial))
        .and_then(|()| rename_dataset(&partial, &ds_name));

    if result.is_err() {
        destroy_quiet(&partial);
    }
    result
}

fn recv_pipeline(
    file_path: &std::path::Path,
    partial: &str,
    compression: Compression,
    zstream: bool,
) -> Result<()> {
    let effective = if zstream {
        Compression::None
    } else {
        compression
    };

    let mut zfs_recv = Command::new(ZFS)
        .args(["receive", partial])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;

    let mut recv_stdin = zfs_recv
        .stdin
        .take()
        .ok_or_else(|| Error::Internal("zfs receive: no stdin".to_string()))?;

    // Drain zfs receive stdout/stderr on background threads so a full
    // pipe can never deadlock the writer.
    let recv_out = zfs_recv.stdout.take();
    let recv_err = zfs_recv.stderr.take();
    let out_thread = recv_out.map(|mut h| {
        std::thread::spawn(move || {
            let mut s = String::new();
            let _ = h.read_to_string(&mut s);
            s
        })
    });
    let err_thread = recv_err.map(|mut h| {
        std::thread::spawn(move || {
            let mut s = String::new();
            let _ = h.read_to_string(&mut s);
            s
        })
    });

    let file = std::fs::File::open(file_path)?;

    // `uncompressor_child` is Some for shelled bzip2/xz.
    let mut uncompressor_child: Option<std::process::Child> = None;
    let mut uncompressor_bin: &str = "";

    let pump_result: Result<()> = match effective {
        Compression::Gzip => {
            let mut dec = flate2::read::MultiGzDecoder::new(file);
            copy_reader(&mut dec, &mut recv_stdin)
        }
        Compression::None => {
            let mut r = file;
            copy_reader(&mut r, &mut recv_stdin)
        }
        Compression::Bzip2 | Compression::Xz => {
            let bin = if matches!(effective, Compression::Bzip2) {
                "/usr/bin/bzip2"
            } else {
                "/usr/bin/xz"
            };
            uncompressor_bin = bin;
            let mut child = Command::new(bin)
                .args(["-cdfq"])
                .stdin(Stdio::from(file))
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()?;
            let mut child_out = child
                .stdout
                .take()
                .ok_or_else(|| Error::Internal("uncompressor: no stdout".to_string()))?;
            let r = copy_reader(&mut child_out, &mut recv_stdin);
            uncompressor_child = Some(child);
            r
        }
    };

    // Close stdin so zfs receive observes EOF.
    drop(recv_stdin);

    // Wait on the uncompressor (if any) before zfs, mirroring the
    // node finish-barrier.
    if let Some(mut child) = uncompressor_child.take() {
        let status = child.wait()?;
        if !status.success() {
            let mut err = String::new();
            if let Some(mut e) = child.stderr.take() {
                let _ = e.read_to_string(&mut err);
            }
            // Still reap zfs receive to avoid a zombie.
            let _ = zfs_recv.wait();
            return Err(Error::Uncompression(format!(
                "{uncompressor_bin} exited with {:?}: {}",
                status.code(),
                err.trim()
            )));
        }
    }

    let status = zfs_recv.wait()?;
    let _ = out_thread.map(|t| t.join());
    let stderr_text = err_thread.and_then(|t| t.join().ok()).unwrap_or_default();

    pump_result?;
    if !status.success() {
        return Err(Error::Internal(format!(
            "zfs receive error while importing: exit code {:?}: {}",
            status.code(),
            stderr_text.trim()
        )));
    }
    Ok(())
}

fn copy_reader<R: Read, W: Write>(r: &mut R, w: &mut W) -> Result<()> {
    let mut buf = vec![0u8; 1 << 20];
    loop {
        let n = match r.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => n,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e.into()),
        };
        w.write_all(&buf[..n])?;
    }
    w.flush()?;
    Ok(())
}

/// Ensure the partial dataset has exactly one snapshot named
/// `@final`, renaming the lone snapshot if necessary.
fn ensure_final_snapshot(uuid: &str, partial: &str) -> Result<()> {
    let out = run(ZFS, &["list", "-t", "snapshot", "-pHr", "-o", "name", partial])?;
    let snaps: Vec<String> = out
        .lines()
        .map(str::trim)
        .filter(|l| l.starts_with(&format!("{partial}@")))
        .map(str::to_string)
        .collect();

    if snaps.len() != 1 {
        let names: Vec<String> = snaps
            .iter()
            .map(|s| format!("@{}", s.rsplit('@').next().unwrap_or("")))
            .collect();
        return Err(Error::UnexpectedSnapshots {
            uuid: uuid.to_string(),
            count: snaps.len(),
            snaps: names.join(", "),
        });
    }

    let snap = &snaps[0];
    if !snap.ends_with("@final") {
        let final_snap = format!("{partial}@final");
        run(ZFS, &["rename", "-r", snap, &final_snap])?;
    }
    Ok(())
}

fn rename_dataset(from: &str, to: &str) -> Result<()> {
    run(ZFS, &["rename", from, to])?;
    Ok(())
}

/// Run a command, returning stdout on success or an error carrying
/// stderr on failure.
fn run(program: &str, args: &[&str]) -> Result<String> {
    let out = Command::new(program).args(args).output()?;
    if !out.status.success() {
        return Err(Error::Internal(format!(
            "{program} {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        )));
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uuid_matching() {
        assert!(is_uuid("01898fb2-aeb3-4d72-8024-67d407b73783"));
        assert!(!is_uuid("01898FB2-aeb3-4d72-8024-67d407b73783"));
        assert!(!is_uuid("not-a-uuid"));
    }

    #[test]
    fn fs_name_matching() {
        assert!(fs_name_re("zones/01898fb2-aeb3-4d72-8024-67d407b73783"));
        assert!(fs_name_re("zones/01898fb2-aeb3-4d72-8024-67d407b73783-disk0"));
        assert!(!fs_name_re(
            "zones/01898fb2-aeb3-4d72-8024-67d407b73783@final"
        ));
        assert!(!fs_name_re("zones/cores"));
    }

    #[test]
    fn img_name_matching() {
        assert!(img_name_re("zones/01898fb2-aeb3-4d72-8024-67d407b73783"));
        assert!(!img_name_re(
            "zones/01898fb2-aeb3-4d72-8024-67d407b73783-disk0"
        ));
    }

    #[test]
    fn split_name() {
        assert_eq!(
            split_fs_name("zones/01898fb2-aeb3-4d72-8024-67d407b73783"),
            Some((
                "zones".to_string(),
                "01898fb2-aeb3-4d72-8024-67d407b73783".to_string()
            ))
        );
        assert_eq!(
            split_fs_name("zones/01898fb2-aeb3-4d72-8024-67d407b73783-disk0"),
            Some((
                "zones".to_string(),
                "01898fb2-aeb3-4d72-8024-67d407b73783".to_string()
            ))
        );
    }
}
