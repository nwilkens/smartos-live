// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.
//
// Copyright 2026 Edgecast Cloud LLC.

//! The NoCloud cloud-init cidata seed: a 16 MiB FAT16 `CIDATA` zvol that
//! carries `meta-data` / `network-config` / `user-data` / `vendor-data`
//! into a bhyve guest. The file *contents* are assembled here (from the
//! payload's `cloud-init:*` metadata, with node's defaults); the FAT image
//! is written to the raw zvol by the host backend
//! ([`crate::backend::ZoneHost::build_nocloud_seed`]) using the native
//! `lofiadm`/`mkfs -F pcfs`/`mount` toolchain, byte-compatible with node's
//! `cloudinit/lofs-fat16.js`.

use serde_json::Value;

use crate::model::spec::{JsonMap, VmCreateSpec};

/// The cidata files to write into the CIDATA volume. `meta-data` and
/// `user-data` are always present (node defaults them); the others only
/// when supplied.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SeedFiles {
    pub meta_data: String,
    pub network_config: Option<String>,
    pub user_data: String,
    pub vendor_data: Option<String>,
}

/// Assemble the cidata file contents from the spec. The agent supplies the
/// three `cloud-init:*` keys in `customer_metadata`; absent ones fall back
/// to node's defaults (`nocloud.js`).
#[must_use]
pub fn build_seed_files(spec: &VmCreateSpec) -> SeedFiles {
    let customer = &spec.customer_metadata;
    let internal = &spec.internal_metadata;

    let meta_data =
        string_meta(customer, "cloud-init:meta-data").unwrap_or_else(|| default_meta_data(spec));
    // node's networkConfig() auto-generates a netplan when absent; the
    // agent always supplies one, so an absent key skips the file rather
    // than risk a divergent auto-generated netplan.
    let network_config = string_meta(customer, "cloud-init:network-config");
    // node always writes at least the FreeBSD-nuageinit-friendly default.
    let user_data = string_meta(customer, "cloud-init:user-data")
        .unwrap_or_else(|| "#cloud-config\n{}\n".to_string());
    let vendor_data = string_meta(internal, "cloud-init:vendor-data");

    SeedFiles {
        meta_data,
        network_config,
        user_data,
        vendor_data,
    }
}

fn string_meta(map: &JsonMap, key: &str) -> Option<String> {
    match map.get(key)? {
        Value::String(s) => Some(s.clone()),
        other => Some(other.to_string()),
    }
}

/// node `metaDataConfig`: `instance-id` + `local-hostname`.
fn default_meta_data(spec: &VmCreateSpec) -> String {
    let hostname = spec
        .hostname
        .clone()
        .or_else(|| spec.alias.clone())
        .unwrap_or_else(|| spec.uuid.to_string());
    format!(
        "# cloud-init meta-data\ninstance-id: {}\nlocal-hostname: {hostname}\n",
        spec.uuid
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::payload::{VmadmCreatePayload, normalize};

    fn spec(json: serde_json::Value) -> VmCreateSpec {
        let p = VmadmCreatePayload::from_json_slice(json.to_string().as_bytes()).unwrap();
        normalize(p, chrono::DateTime::from_timestamp(0, 0).unwrap()).unwrap()
    }

    #[test]
    fn uses_supplied_cloud_init_keys() {
        let s = spec(serde_json::json!({
            "uuid": "11111111-1111-1111-1111-111111111111",
            "brand": "bhyve", "ram": 512,
            "customer_metadata": {
                "cloud-init:user-data": "#cloud-config\nssh_pwauth: true\n",
                "cloud-init:meta-data": "instance-id: custom\n",
                "cloud-init:network-config": "version: 2\n"
            }
        }));
        let files = build_seed_files(&s);
        assert_eq!(files.user_data, "#cloud-config\nssh_pwauth: true\n");
        assert_eq!(files.meta_data, "instance-id: custom\n");
        assert_eq!(files.network_config.as_deref(), Some("version: 2\n"));
        assert!(files.vendor_data.is_none());
    }

    #[test]
    fn defaults_when_absent() {
        let s = spec(serde_json::json!({
            "uuid": "11111111-1111-1111-1111-111111111111",
            "brand": "bhyve", "ram": 512, "hostname": "web0"
        }));
        let files = build_seed_files(&s);
        assert_eq!(files.user_data, "#cloud-config\n{}\n");
        assert_eq!(
            files.meta_data,
            "# cloud-init meta-data\ninstance-id: 11111111-1111-1111-1111-111111111111\nlocal-hostname: web0\n"
        );
        assert!(files.network_config.is_none());
    }
}
