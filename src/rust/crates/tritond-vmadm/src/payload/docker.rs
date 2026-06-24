// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.
//
// Copyright 2026 Edgecast Cloud LLC.

//! The docker create derivation (node `VM.js:10085-10148`). A "docker zone"
//! is NOT a brand: it is an lx (or joyent-minimal) zone with the `docker`
//! flag plus `docker:*` internal_metadata, run by the in-zone C dockerinit
//! (src/dockerinit/README.md:13). When `docker=true` the engine derives:
//!   1. `docker` is added to `internal_metadata_namespaces` so the keys are
//!      GZ-controlled and immutable from inside the zone (VM.js:10097-10103);
//!   2. `restart_init` is forced false so zoneadmd does not restart the
//!      container init (OS-3546, VM.js:10108-10112);
//!   3. `init_name` is set to the brand's `features.dockerinit` path
//!      (VM.js:10116-10122, proptable.js:2420/2454);
//!   4. a `docker:id` (64-char hex) is generated once if absent
//!      (VM.js:10138, newDockerId).
//!
//! It also computes the docker log attrs `zlog-mode` / `app-svc-dependent`
//! (`buildDockerLogZonecfg` / `calculateZlogModeAndDriver`, VM.js:6596/19523).

use serde_json::Value;
use uuid::Uuid;

use crate::brand::Brand;
use crate::error::VmadmError;
use crate::model::spec::JsonMap;

/// The docker-derived create state, computed from the create payload's
/// `docker` flag + `internal_metadata` and the target brand.
pub struct DockerDerived {
    pub restart_init: Option<bool>,
    pub init_name: Option<String>,
    pub internal_metadata_namespaces: Vec<String>,
    pub zlog_mode: Option<String>,
    pub app_svc_dependent: bool,
}

/// `newDockerId(uuid)` (VM.js:17151): the zone uuid concatenated with a
/// fresh v4 uuid, hyphens stripped, yielding a 64-char hex string. This is
/// the docker container id sdc-docker round-trips, so the format must match
/// node exactly (32 + 32 hex chars).
#[must_use]
pub fn new_docker_id(uuid: Uuid) -> String {
    let extra = Uuid::new_v4();
    format!("{}{}", uuid.simple(), extra.simple())
}

/// Derive the docker create fields (VM.js:10085-10148) and mutate
/// `internal_metadata` to carry a generated `docker:id` when absent.
///
/// `brand` must be docker-capable (caller validates); `init_name` /
/// `restart_init` are the explicit payload values (an explicit value wins
/// over the docker default, matching node's `!payload.init_name` /
/// `payload.restart_init !== false` guards).
pub fn derive(
    brand: Brand,
    payload_init_name: Option<String>,
    payload_restart_init: Option<bool>,
    payload_namespaces: &[String],
    uuid: Uuid,
    internal_metadata: &mut JsonMap,
) -> Result<DockerDerived, VmadmError> {
    // (1) internal_metadata_namespaces gains `docker` (VM.js:10097-10103).
    let mut namespaces: Vec<String> = payload_namespaces.to_vec();
    if !namespaces.iter().any(|n| n == "docker") {
        namespaces.push("docker".to_string());
    }

    // (2) restart_init forced false (VM.js:10108-10112). node's guard only
    // ever sets it false: when the existing/payload value is already false it
    // is a no-op, otherwise it forces false. A docker zone is always
    // restart_init=false so zoneadmd never restarts the container init. An
    // explicit payload restart_init=false is consistent; restart_init=true is
    // overridden to false (node's intent), which we record here.
    let _ = payload_restart_init;
    let restart_init = Some(false);

    // (3) init_name from features.dockerinit unless the payload set one
    // (VM.js:10116-10122). The brand is docker-capable so dockerinit is Some.
    let init_name = match payload_init_name {
        Some(n) => Some(n),
        None => brand.features().dockerinit.map(str::to_string),
    };

    // (4) docker:id generated once if absent (VM.js:10134-10146). Never
    // auto-removed; preserved across updates.
    if !internal_metadata.contains_key("docker:id") {
        internal_metadata.insert(
            "docker:id".to_string(),
            Value::String(new_docker_id(uuid)),
        );
    }

    // Log attrs (buildDockerLogZonecfg, VM.js:6596 / calculateZlogModeAndDriver,
    // VM.js:19523) derived from docker:tty + docker:logdriver.
    let (zlog_mode, app_svc_dependent) = calculate_zlog(internal_metadata);

    Ok(DockerDerived {
        restart_init,
        init_name,
        internal_metadata_namespaces: namespaces,
        zlog_mode: Some(zlog_mode),
        app_svc_dependent,
    })
}

/// `calculateZlogModeAndDriver` + `zlogObjToString` (VM.js:19523/19498) for
/// the create path (payload-only; no prior vmobj). Returns the 3-char
/// zlog-mode string (`g`/`-`, `t`/`-`, `n`/`-`) and whether the log driver
/// requires `app-svc-dependent=true`.
fn calculate_zlog(internal_metadata: &JsonMap) -> (String, bool) {
    // tty bit from docker:tty (VM.js:19537-19541, `!!` truthiness).
    let tty = internal_metadata
        .get("docker:tty")
        .map(is_truthy)
        .unwrap_or(false);

    // log driver from docker:logdriver, defaulting to json-file
    // (VM.js:19555-19573).
    let driver = internal_metadata
        .get("docker:logdriver")
        .and_then(Value::as_str)
        .unwrap_or("json-file");

    // gz/ngz bits per driver (VM.js:19577-19587). json-file => gz only;
    // none => neither; any other (logger-backed) => gz + ngz.
    let (gz, ngz) = match driver {
        "json-file" => (true, false),
        "none" => (false, false),
        _ => (true, true),
    };

    let mode = format!(
        "{}{}{}",
        if gz { "g" } else { "-" },
        if tty { "t" } else { "-" },
        if ngz { "n" } else { "-" },
    );

    // app-svc-dependent is set true only for logger-backed drivers
    // (VM.js:6627-6636): json-file/none do not spawn a logger child.
    let app_svc_dependent = !matches!(driver, "json-file" | "none");

    (mode, app_svc_dependent)
}

/// node truthiness (`!!value`) for a docker:* metadata value: a JSON bool
/// uses its value; a string is truthy when non-empty and not "false"/"0"
/// (sdc-docker stores docker:tty as the string "true"/"false"); a number is
/// truthy when non-zero.
fn is_truthy(v: &Value) -> bool {
    match v {
        Value::Bool(b) => *b,
        Value::String(s) => !s.is_empty() && s != "false" && s != "0",
        Value::Number(n) => n.as_f64().map(|f| f != 0.0).unwrap_or(false),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn uuid() -> Uuid {
        Uuid::parse_str("00000000-0000-0000-0000-000000000001").unwrap()
    }

    #[test]
    fn docker_id_is_64_hex_chars() {
        let id = new_docker_id(uuid());
        assert_eq!(id.len(), 64);
        assert!(id.chars().all(|c| c.is_ascii_hexdigit()));
        // First 32 chars are the zone uuid (hyphens stripped, lower-case).
        assert!(id.starts_with("00000000000000000000000000000001"));
    }

    #[test]
    fn derive_sets_namespaces_restart_init_and_init_name_for_lx() {
        let mut imd = JsonMap::new();
        let d = derive(Brand::Lx, None, None, &[], uuid(), &mut imd).unwrap();
        assert_eq!(d.internal_metadata_namespaces, vec!["docker".to_string()]);
        assert_eq!(d.restart_init, Some(false));
        assert_eq!(d.init_name.as_deref(), Some("/native/usr/vm/sbin/dockerinit"));
        // docker:id stamped once.
        assert!(imd.get("docker:id").and_then(Value::as_str).is_some());
    }

    #[test]
    fn derive_init_name_for_joyent_minimal() {
        let mut imd = JsonMap::new();
        let d = derive(Brand::JoyentMinimal, None, None, &[], uuid(), &mut imd).unwrap();
        assert_eq!(d.init_name.as_deref(), Some("/usr/vm/sbin/dockerinit"));
    }

    #[test]
    fn derive_preserves_existing_docker_id() {
        let mut imd = JsonMap::new();
        imd.insert(
            "docker:id".to_string(),
            Value::String("deadbeef".repeat(8)),
        );
        let _ = derive(Brand::Lx, None, None, &[], uuid(), &mut imd).unwrap();
        assert_eq!(
            imd.get("docker:id").and_then(Value::as_str),
            Some("deadbeefdeadbeefdeadbeefdeadbeefdeadbeefdeadbeefdeadbeefdeadbeef")
        );
    }

    #[test]
    fn derive_honors_explicit_init_name() {
        let mut imd = JsonMap::new();
        let d = derive(
            Brand::Lx,
            Some("/custom/init".to_string()),
            None,
            &[],
            uuid(),
            &mut imd,
        )
        .unwrap();
        assert_eq!(d.init_name.as_deref(), Some("/custom/init"));
    }

    #[test]
    fn derive_does_not_duplicate_docker_namespace() {
        let mut imd = JsonMap::new();
        let d = derive(
            Brand::Lx,
            None,
            None,
            &["docker".to_string()],
            uuid(),
            &mut imd,
        )
        .unwrap();
        assert_eq!(d.internal_metadata_namespaces, vec!["docker".to_string()]);
    }

    #[test]
    fn zlog_default_is_g_only_no_app_svc() {
        // No docker:tty, no docker:logdriver => json-file => "g--", no
        // app-svc-dependent.
        let imd = JsonMap::new();
        let (mode, app) = calculate_zlog(&imd);
        assert_eq!(mode, "g--");
        assert!(!app);
    }

    #[test]
    fn zlog_tty_sets_t_bit() {
        let mut imd = JsonMap::new();
        imd.insert("docker:tty".to_string(), Value::String("true".to_string()));
        let (mode, app) = calculate_zlog(&imd);
        assert_eq!(mode, "gt-");
        assert!(!app);
    }

    #[test]
    fn zlog_none_driver_is_dashes() {
        let mut imd = JsonMap::new();
        imd.insert(
            "docker:logdriver".to_string(),
            Value::String("none".to_string()),
        );
        let (mode, app) = calculate_zlog(&imd);
        assert_eq!(mode, "---");
        assert!(!app);
    }

    #[test]
    fn zlog_logger_driver_sets_ngz_and_app_svc() {
        // A logger-backed driver (e.g. syslog) writes GZ+NGZ and needs
        // app-svc-dependent so init dies with the logger (VM.js:6633).
        let mut imd = JsonMap::new();
        imd.insert(
            "docker:logdriver".to_string(),
            Value::String("syslog".to_string()),
        );
        imd.insert("docker:tty".to_string(), Value::Bool(true));
        let (mode, app) = calculate_zlog(&imd);
        assert_eq!(mode, "gtn");
        assert!(app);
    }
}
