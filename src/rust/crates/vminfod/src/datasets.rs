// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.
//
// Copyright 2026 Edgecast Cloud LLC.

//! Delegated-dataset enrichment for the VM object.
//!
//! node's vmload adds three conditional keys when a zone has a delegated
//! ZFS dataset (a zonecfg `add dataset` resource, conventionally
//! `zones/<uuid>/data`):
//!
//!   - `datasets`: the array of delegated dataset names (always present when
//!     a delegated dataset exists).
//!   - `zfs_data_recsize`: the `recordsize` of the `data` dataset (always
//!     present alongside `datasets`).
//!   - `zfs_data_compression`: the `compression` of the `data` dataset, but
//!     OMITTED when it is `off` (node only surfaces a non-`off` value).
//!
//! `tritond_vmadm::get` does not model these, so we enrich the VM value here
//! using the same `ZoneHost` the loader already drives: the zonecfg export
//! supplies the `dataset` resource names, and `zfs get` supplies the props.

use serde_json::{Map, Value, json};
use tritond_vmadm::backend::ZoneHost;
use uuid::Uuid;

/// Enrich a VM source map (pre-reorder) with the delegated-dataset keys.
/// `src` is the serialized `VmGetJson` object; we insert `datasets`,
/// `zfs_data_recsize`, and (conditionally) `zfs_data_compression` so the
/// reorder step can place them in node's slots.
pub async fn enrich<H: ZoneHost>(host: &H, uuid: Uuid, src: &mut Map<String, Value>) {
    let names = delegated_datasets(host, uuid).await;
    if names.is_empty() {
        return;
    }

    src.insert(
        "datasets".to_string(),
        Value::Array(names.iter().map(|n| Value::String(n.clone())).collect()),
    );

    // node's zfs_data_* come from the `data` dataset specifically.
    let data_ds = names
        .iter()
        .find(|n| n.ends_with("/data"))
        .or_else(|| names.first());

    if let Some(ds) = data_ds {
        let props = host
            .zfs_get_props(ds, &["compression", "recordsize"])
            .await
            .unwrap_or_default();

        // compression: omitted when "off" (node only surfaces non-off).
        if let Some(comp) = props.get("compression")
            && comp != "off"
        {
            src.insert("zfs_data_compression".to_string(), json!(comp));
        }
        if let Some(recsize) = props.get("recordsize").and_then(|s| s.parse::<u64>().ok()) {
            src.insert("zfs_data_recsize".to_string(), json!(recsize));
        }
    }
}

/// The delegated dataset names from the zonecfg export's `dataset` resources.
async fn delegated_datasets<H: ZoneHost>(host: &H, uuid: Uuid) -> Vec<String> {
    let Ok(export) = host.zonecfg_export(uuid).await else {
        return Vec::new();
    };
    parse_dataset_names(&export)
}

/// Parse `add dataset` ... `set name=<ds>` ... `end` blocks from a zonecfg
/// export, returning the dataset names in order.
fn parse_dataset_names(export: &str) -> Vec<String> {
    let mut names = Vec::new();
    let mut in_dataset = false;
    for raw in export.lines() {
        let line = raw.trim();
        if line == "add dataset" {
            in_dataset = true;
            continue;
        }
        if in_dataset {
            if line == "end" {
                in_dataset = false;
                continue;
            }
            if let Some(rest) = line.strip_prefix("set name=") {
                names.push(rest.trim().trim_matches('"').to_string());
            }
        }
    }
    names
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_delegated_dataset() {
        let export = "create -b\n\
            set zonepath=/zones/x\n\
            set brand=joyent-minimal\n\
            add dataset\n\
            set name=zones/x/data\n\
            end\n";
        assert_eq!(parse_dataset_names(export), vec!["zones/x/data".to_string()]);
    }

    #[test]
    fn no_dataset_yields_empty() {
        let export = "create -b\nset brand=joyent-minimal\n";
        assert!(parse_dataset_names(export).is_empty());
    }

    #[test]
    fn quoted_name() {
        let export = "add dataset\nset name=\"zones/y/data\"\nend\n";
        assert_eq!(parse_dataset_names(export), vec!["zones/y/data".to_string()]);
    }
}
