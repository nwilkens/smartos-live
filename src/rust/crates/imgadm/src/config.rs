// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.
//
// Copyright 2026 Edgecast Cloud LLC.

//! `/var/imgadm/imgadm.conf` config and the image-source list.
//!
//! The config file is opaque JSON with a known `sources` key. We
//! preserve any other keys (`dockerImportSkipUuids`, `upgradedToVer`)
//! verbatim so a save round-trips the live config unchanged.

use std::path::PathBuf;

use serde_json::{Map, Value};

use crate::error::{Error, Result};

pub const DB_DIR: &str = "/var/imgadm";
pub const DEFAULT_ZPOOL: &str = "zones";

/// The implicit default source used when the config has no `sources`.
pub fn default_source() -> Source {
    Source {
        ty: "imgapi".to_string(),
        url: "https://images.smartos.org".to_string(),
        insecure: None,
        channel: None,
    }
}

pub fn config_path() -> PathBuf {
    PathBuf::from(DB_DIR).join("imgadm.conf")
}

/// An image source. `insecure`/`channel` are optional and omitted
/// from the serialized form when absent (matching node `toJSON`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Source {
    pub ty: String,
    pub url: String,
    pub insecure: Option<bool>,
    pub channel: Option<String>,
}

impl Source {
    /// `Source.toString()`: `"<type>" image source "<url>"<extra>`.
    pub fn display(&self) -> String {
        let mut extra = String::new();
        if self.insecure == Some(true) {
            extra.push_str(" (insecure)");
        }
        if let Some(ch) = &self.channel {
            extra.push_str(&format!(" (channel={ch})"));
        }
        format!("\"{}\" image source \"{}\"{extra}", self.ty, self.url)
    }

    /// `Source.toJSON()` shape stored in the config `sources` array.
    pub fn to_json(&self) -> Value {
        let mut m = Map::new();
        m.insert("type".to_string(), Value::String(self.ty.clone()));
        m.insert("url".to_string(), Value::String(self.url.clone()));
        if let Some(i) = self.insecure {
            m.insert("insecure".to_string(), Value::Bool(i));
        }
        if let Some(c) = &self.channel {
            m.insert("channel".to_string(), Value::String(c.clone()));
        }
        Value::Object(m)
    }

    /// Normalized URL for de-dup (node `normUrlFromUrl`): ensure a
    /// scheme, strip a trailing `/`, drop redundant default ports.
    pub fn norm_url(&self) -> String {
        norm_url(&self.url)
    }
}

/// Node `normUrlFromUrl`, simplified for the shapes imgadm sees.
pub fn norm_url(u: &str) -> String {
    let mut s = u.to_string();
    if !has_scheme(&s) {
        s = format!("http://{s}");
    }
    // Strip a single trailing slash from the path portion.
    if s.ends_with('/') && !s.ends_with("://") {
        s.pop();
    }
    // Drop redundant default ports.
    s = s.replace(":443", "").replace(":80", "");
    s
}

fn has_scheme(u: &str) -> bool {
    if let Some(idx) = u.find("://") {
        u[..idx].chars().all(|c| c.is_ascii_alphanumeric())
    } else {
        false
    }
}

/// The parsed imgadm config. `raw` holds every key so we can write
/// it back unchanged; `sources` is the typed view of `raw.sources`.
#[derive(Debug, Clone)]
pub struct Config {
    pub raw: Map<String, Value>,
    /// Live source list. Starts from `raw.sources`, or the default
    /// source when `raw.sources` is absent.
    pub sources: Vec<Source>,
    /// Whether `raw.sources` was present (vs. implicit default).
    pub sources_explicit: bool,
}

impl Config {
    pub fn load() -> Result<Config> {
        let path = config_path();
        let (raw, explicit, sources) = match std::fs::read(&path) {
            Ok(bytes) => {
                let raw: Map<String, Value> = serde_json::from_slice(&bytes)
                    .map_err(|e| {
                        Error::Internal(format!(
                            "config file \"{}\" is not valid JSON: {e}",
                            path.display()
                        ))
                    })?;
                match raw.get("sources").and_then(Value::as_array) {
                    Some(arr) => {
                        let srcs = arr.iter().map(source_from_value).collect();
                        (raw, true, srcs)
                    }
                    None => (raw, false, vec![default_source()]),
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                (Map::new(), false, vec![default_source()])
            }
            Err(e) => return Err(Error::Io(e)),
        };
        Ok(Config {
            raw,
            sources,
            sources_explicit: explicit,
        })
    }

    /// Persist the current `sources` into `raw.sources` and write the
    /// config file (pretty, no trailing newline — matching node).
    pub fn save(&mut self) -> Result<()> {
        let arr: Vec<Value> = self.sources.iter().map(Source::to_json).collect();
        self.raw
            .insert("sources".to_string(), Value::Array(arr));
        self.sources_explicit = true;
        let path = config_path();
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let str = serde_json::to_string_pretty(&Value::Object(self.raw.clone()))?;
        std::fs::write(&path, str)?;
        Ok(())
    }

    /// Add a source if not already present (de-dup on
    /// normUrl+type+insecure+channel). Returns true if added.
    pub fn add_source(&mut self, src: Source) -> bool {
        let norm = src.norm_url();
        for s in &self.sources {
            if s.norm_url() == norm
                && s.ty == src.ty
                && s.insecure == src.insecure
                && s.channel == src.channel
            {
                return false;
            }
        }
        self.sources.push(src);
        true
    }

    /// Delete sources matching a (normalized) URL. Returns the
    /// removed sources.
    pub fn del_source_url(&mut self, url: &str) -> Vec<Source> {
        let norm = norm_url(url);
        let mut deleted = Vec::new();
        self.sources.retain(|s| {
            if s.norm_url() == norm {
                deleted.push(s.clone());
                false
            } else {
                true
            }
        });
        deleted
    }
}

fn source_from_value(v: &Value) -> Source {
    Source {
        ty: v
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or("imgapi")
            .to_string(),
        url: v
            .get("url")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        insecure: v.get("insecure").and_then(Value::as_bool),
        channel: v
            .get("channel")
            .and_then(Value::as_str)
            .map(str::to_string),
    }
}
