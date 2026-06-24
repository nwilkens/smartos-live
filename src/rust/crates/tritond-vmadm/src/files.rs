// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.
//
// Copyright 2026 Edgecast Cloud LLC.

//! The per-zone on-disk config files node `vmadm` materializes at create
//! and `vmadm get` reads back: `<zonepath>/config/{metadata,tags,routes}.json`
//! plus the `<zonepath>/root/var/svc/provisioning` marker. Writes are
//! atomic (write to a temp sibling, then rename), matching `writeAndRename`.

use std::path::{Path, PathBuf};

use serde::Serialize;
use tokio::io::AsyncWriteExt as _;

use crate::constants::PROVISIONING_MARKER;
use crate::error::{Result, VmadmError};
use crate::model::spec::JsonMap;

/// `metadata.json` envelope. Field order is `internal_metadata` then
/// `customer_metadata` to match node (`writeAllMetadata` inserts
/// `d.internal_metadata` before `d.customer_metadata`). Inner-map key
/// order follows serde's `Map` (sorted) rather than node's insertion
/// order; this is semantically equal and the differential harness
/// compares these maps by value.
#[derive(Serialize)]
struct MetadataFile<'a> {
    internal_metadata: &'a JsonMap,
    customer_metadata: &'a JsonMap,
}

/// Write the three config files at create time. `metadata.json` is always
/// written (both keys); `tags.json` / `routes.json` only when non-empty
/// (node skips an unchanged/empty map for a new zone).
pub async fn write_create_config(
    zonepath: &Path,
    customer_metadata: &JsonMap,
    internal_metadata: &JsonMap,
    tags: &JsonMap,
    routes: &JsonMap,
) -> Result<()> {
    let config_dir = zonepath.join("config");
    assert_no_symlink_escape(zonepath, &config_dir).await?;
    recreate_dir(&config_dir).await?;

    let metadata = MetadataFile {
        internal_metadata,
        customer_metadata,
    };
    write_and_rename(&config_dir.join("metadata.json"), &to_pretty(&metadata)?).await?;

    // node's createConfigDir lays down `{}` skeletons for all three files,
    // then writeAllMetadata overwrites the non-empty ones. Match that so a
    // reader (or `vmadm get`) always finds the files present.
    write_config_map(&config_dir.join("tags.json"), tags).await?;
    write_config_map(&config_dir.join("routes.json"), routes).await?;
    Ok(())
}

/// Write a config map file: the pretty JSON object when non-empty, else
/// the `{}\n` skeleton node leaves in place.
async fn write_config_map(path: &Path, map: &JsonMap) -> Result<()> {
    let body = if map.is_empty() {
        "{}\n".to_string()
    } else {
        to_pretty(map)?
    };
    write_and_rename(path, &body).await
}

/// Write the OS-zone network files into the zoneroot (`writeZoneNetfiles`):
/// `/etc/hostname.<if>` (one `<ip> up` line per static IP),
/// `/etc/defaultrouter` (primary NIC's gateways), `/etc/dhcp.<if>` /
/// `/etc/addrconf.<if>` markers, and `/etc/nodename`. The zoneroot exists
/// after `zoneadm install`.
pub async fn write_zone_netfiles(
    zonepath: &Path,
    nics: &[crate::model::nic::NicSpec],
    hostname: &str,
) -> Result<()> {
    use crate::model::nic::NicAddr;
    let etc = zonepath.join("root/etc");
    assert_no_symlink_escape(zonepath, &etc).await?;
    mkdir_p(&etc).await?;

    let mut primary_done = false;
    for nic in nics {
        let mut static_ips = String::new();
        let (mut has_dhcp, mut has_addrconf) = (false, false);
        for addr in &nic.ips {
            match addr {
                NicAddr::Dhcp => has_dhcp = true,
                NicAddr::Addrconf => has_addrconf = true,
                NicAddr::Net(n) => {
                    static_ips.push_str(&n.to_string());
                    static_ips.push_str(" up\n");
                }
            }
        }
        if !static_ips.is_empty() {
            write_and_rename(
                &etc.join(format!("hostname.{}", nic.interface)),
                &static_ips,
            )
            .await?;
        }
        if has_addrconf {
            write_and_rename(&etc.join(format!("addrconf.{}", nic.interface)), "").await?;
        }
        if nic.primary && !primary_done {
            primary_done = true;
            if !nic.gateways.is_empty() {
                let routers = nic
                    .gateways
                    .iter()
                    .map(ToString::to_string)
                    .collect::<Vec<_>>()
                    .join("\n");
                write_and_rename(&etc.join("defaultrouter"), &format!("{routers}\n")).await?;
            }
            if has_dhcp {
                write_and_rename(&etc.join(format!("dhcp.{}", nic.interface)), "").await?;
            }
        }
    }

    write_and_rename(&etc.join("nodename"), &format!("{hostname}\n")).await
}

/// Create the empty `<zonepath>/root/var/svc/provisioning` marker that the
/// in-zone mdata SMF replaces with `provision_success`/`provision_failure`.
pub async fn write_provisioning_marker(zonepath: &Path) -> Result<()> {
    let marker = zonepath.join(PROVISIONING_MARKER);
    assert_no_symlink_escape(zonepath, &marker).await?;
    if let Some(parent) = marker.parent() {
        mkdir_p(parent).await?;
    }
    write_and_rename(&marker, "").await
}

/// Write the kvm `/root/startvm` qemu launch script and mark it executable
/// (0755), the way node `VM.js` writeKVMStartScript does (VM.js:13868). The
/// kvm brand's boot hook `exec`s this script; without it `zoneadm boot` fails
/// with "could not stat .../root/startvm". Rejects a symlinked path component
/// (image-escape defense) before writing.
pub async fn write_startvm(zonepath: &Path, script: &str) -> Result<()> {
    use std::os::unix::fs::PermissionsExt as _;
    let target = zonepath.join("root/startvm");
    assert_no_symlink_escape(zonepath, &target).await?;
    if let Some(parent) = target.parent() {
        mkdir_p(parent).await?;
    }
    write_and_rename(&target, script).await?;
    tokio::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o755))
        .await
        .map_err(|e| io_err(&target, e))
}

/// `JSON.stringify(x, null, 2)` equivalent: 2-space pretty, no trailing
/// newline (node's `writeAndRename` writes the string verbatim).
fn to_pretty<T: Serialize>(value: &T) -> Result<String> {
    serde_json::to_string_pretty(value)
        .map_err(|e| VmadmError::Payload(format!("serialize config json: {e}")))
}

/// Reject writing through a symlinked path component under the zone. A
/// malicious image could plant e.g. `root/etc -> /etc` so that a root-owned
/// write into the zoneroot escapes into the global zone. Walks every
/// EXISTING component from `zonepath` to `target` and refuses if any is a
/// symlink. (Not TOCTOU-proof — a full fix uses `openat`/`O_NOFOLLOW`
/// per component — but it blocks the static-symlink-in-image attack.)
async fn assert_no_symlink_escape(zonepath: &Path, target: &Path) -> Result<()> {
    let rel = target.strip_prefix(zonepath).map_err(|_| {
        VmadmError::Payload(format!(
            "internal: {} is not under zonepath {}",
            target.display(),
            zonepath.display()
        ))
    })?;
    let mut cur = zonepath.to_path_buf();
    for comp in rel.components() {
        cur.push(comp);
        // Only existing components matter (the leaf may not exist yet).
        if let Ok(md) = tokio::fs::symlink_metadata(&cur).await
            && md.file_type().is_symlink()
        {
            return Err(VmadmError::Payload(format!(
                "refusing to write through symlinked path component {} (possible image escape)",
                cur.display()
            )));
        }
    }
    Ok(())
}

async fn recreate_dir(dir: &Path) -> Result<()> {
    if tokio::fs::try_exists(dir).await.unwrap_or(false) {
        tokio::fs::remove_dir_all(dir)
            .await
            .map_err(|e| io_err(dir, e))?;
    }
    mkdir_p(dir).await
}

async fn mkdir_p(dir: &Path) -> Result<()> {
    tokio::fs::create_dir_all(dir)
        .await
        .map_err(|e| io_err(dir, e))
}

/// Atomic write: write to `<path>.<n>.tmp`, fsync, rename over `path`.
pub(crate) async fn write_and_rename(path: &Path, contents: &str) -> Result<()> {
    let tmp = tmp_sibling(path);
    {
        let mut f = tokio::fs::File::create(&tmp)
            .await
            .map_err(|e| io_err(&tmp, e))?;
        f.write_all(contents.as_bytes())
            .await
            .map_err(|e| io_err(&tmp, e))?;
        f.sync_all().await.map_err(|e| io_err(&tmp, e))?;
    }
    tokio::fs::rename(&tmp, path)
        .await
        .map_err(|e| io_err(path, e))
}

/// A temp sibling path. The pid keeps concurrent writers from colliding;
/// the zone lock (see `lock.rs`) already serializes same-zone writers, so
/// this is belt-and-suspenders.
fn tmp_sibling(path: &Path) -> PathBuf {
    let mut name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "tmp".to_string());
    name.push_str(&format!(".{}.tmp", std::process::id()));
    path.with_file_name(name)
}

fn io_err(path: &Path, source: std::io::Error) -> VmadmError {
    VmadmError::Io {
        path: path.to_path_buf(),
        source,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn map(v: serde_json::Value) -> JsonMap {
        v.as_object().unwrap().clone()
    }

    #[tokio::test]
    async fn metadata_json_envelope_internal_first_2space() {
        let dir = tempfile::tempdir().unwrap();
        let zonepath = dir.path();
        let customer = map(json!({ "root_authorized_keys": "ssh-ed25519 AAAA" }));
        let internal = map(json!({ "tritond:instance_id": "abc" }));
        write_create_config(
            zonepath,
            &customer,
            &internal,
            &JsonMap::new(),
            &JsonMap::new(),
        )
        .await
        .unwrap();

        let got = std::fs::read_to_string(zonepath.join("config/metadata.json")).unwrap();
        let want = "{\n  \"internal_metadata\": {\n    \"tritond:instance_id\": \"abc\"\n  },\n  \"customer_metadata\": {\n    \"root_authorized_keys\": \"ssh-ed25519 AAAA\"\n  }\n}";
        assert_eq!(got, want);
        // tags.json / routes.json are written as `{}` skeletons when empty
        // (matching node's createConfigDir).
        assert_eq!(
            std::fs::read_to_string(zonepath.join("config/tags.json")).unwrap(),
            "{}\n"
        );
        assert_eq!(
            std::fs::read_to_string(zonepath.join("config/routes.json")).unwrap(),
            "{}\n"
        );
    }

    #[tokio::test]
    async fn tags_json_written_when_present() {
        let dir = tempfile::tempdir().unwrap();
        let tags = map(json!({ "tritond.instance_id": "i-1" }));
        write_create_config(
            dir.path(),
            &JsonMap::new(),
            &JsonMap::new(),
            &tags,
            &JsonMap::new(),
        )
        .await
        .unwrap();
        let got = std::fs::read_to_string(dir.path().join("config/tags.json")).unwrap();
        assert_eq!(got, "{\n  \"tritond.instance_id\": \"i-1\"\n}");
    }

    #[tokio::test]
    async fn provisioning_marker_is_empty_file() {
        let dir = tempfile::tempdir().unwrap();
        write_provisioning_marker(dir.path()).await.unwrap();
        let marker = dir.path().join("root/var/svc/provisioning");
        assert!(marker.exists());
        assert_eq!(std::fs::read_to_string(&marker).unwrap(), "");
    }

    #[tokio::test]
    async fn config_dir_recreated_clean() {
        let dir = tempfile::tempdir().unwrap();
        let config = dir.path().join("config");
        std::fs::create_dir_all(&config).unwrap();
        std::fs::write(config.join("stale.json"), "old").unwrap();
        write_create_config(
            dir.path(),
            &JsonMap::new(),
            &JsonMap::new(),
            &JsonMap::new(),
            &JsonMap::new(),
        )
        .await
        .unwrap();
        // The stale file is gone after recreate.
        assert!(!config.join("stale.json").exists());
        assert!(config.join("metadata.json").exists());
    }
}
