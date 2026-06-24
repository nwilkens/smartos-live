// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.
//
// Copyright 2026 Edgecast Cloud LLC.

//! The imgadm db under `/var/imgadm/images/`.
//!
//! One file per installed image: `<zpool>-<uuid>.json` holding
//! `{manifest, zpool, source?}`. The db is a best-effort cache: a
//! missing/corrupt file yields the minimal `{manifest:{uuid}, zpool}`
//! and never errors (node `loadImage` semantics).

use std::path::PathBuf;

use serde_json::{Map, Value};

use crate::config::DB_DIR;
use crate::error::Result;
use crate::imageinfo::ImageInfo;

fn db_image_path(zpool: &str, uuid: &str) -> PathBuf {
    PathBuf::from(DB_DIR)
        .join("images")
        .join(format!("{zpool}-{uuid}.json"))
}

/// Load cached image info. Never errors: a missing or corrupt file
/// returns the minimal manifest. The on-disk `manifest.uuid` is
/// trusted to match `uuid` (node asserts this; we tolerate drift by
/// preferring the requested uuid in the minimal fallback).
pub fn load_image(zpool: &str, uuid: &str) -> ImageInfo {
    let path = db_image_path(zpool, uuid);
    if let Ok(bytes) = std::fs::read(&path)
        && let Ok(Value::Object(obj)) = serde_json::from_slice::<Value>(&bytes)
        && obj
            .get("manifest")
            .and_then(|m| m.get("uuid"))
            .and_then(Value::as_str)
            == Some(uuid)
    {
        let manifest = obj
            .get("manifest")
            .cloned()
            .unwrap_or_else(|| minimal_manifest(uuid));
        let zpool_v = obj
            .get("zpool")
            .and_then(Value::as_str)
            .map(str::to_string)
            .unwrap_or_else(|| zpool.to_string());
        let source = obj
            .get("source")
            .and_then(Value::as_str)
            .map(str::to_string);
        // Preserve the db file's exact key order so `list -j`/`get`
        // round-trip byte-for-byte (node echoes the parsed object).
        return ImageInfo {
            manifest,
            zpool: Some(zpool_v),
            source,
            clones: None,
            clone_names: None,
            children: None,
            base: Some(obj),
        };
    }
    // Minimal fallback: node returns `{manifest: {uuid}, zpool}`.
    let mut base = Map::new();
    base.insert("manifest".to_string(), minimal_manifest(uuid));
    base.insert("zpool".to_string(), Value::String(zpool.to_string()));
    ImageInfo {
        manifest: minimal_manifest(uuid),
        zpool: Some(zpool.to_string()),
        source: None,
        clones: None,
        clone_names: None,
        children: None,
        base: Some(base),
    }
}

fn minimal_manifest(uuid: &str) -> Value {
    let mut m = Map::new();
    m.insert("uuid".to_string(), Value::String(uuid.to_string()));
    Value::Object(m)
}

/// Persist image info. Writes `{manifest, zpool, source?}` pretty
/// JSON with a trailing newline (node `addImage`).
pub fn add_image(zpool: &str, manifest: &Value, source_url: Option<&str>) -> Result<()> {
    let uuid = manifest
        .get("uuid")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let path = db_image_path(zpool, uuid);
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let mut obj = Map::new();
    obj.insert("manifest".to_string(), manifest.clone());
    obj.insert("zpool".to_string(), Value::String(zpool.to_string()));
    // node writes `source: undefined` which JSON.stringify drops, so
    // we only include the key when a source URL is present.
    if let Some(s) = source_url {
        obj.insert("source".to_string(), Value::String(s.to_string()));
    }
    let mut content = serde_json::to_string_pretty(&Value::Object(obj))?;
    content.push('\n');
    std::fs::write(&path, content)?;
    Ok(())
}

/// Delete the cached image file. Not an error if it does not exist.
pub fn delete_image(zpool: &str, uuid: &str) -> Result<()> {
    let path = db_image_path(zpool, uuid);
    match std::fs::remove_file(&path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e.into()),
    }
}
