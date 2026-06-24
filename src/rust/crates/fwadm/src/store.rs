// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.
//
// Copyright 2026 Edgecast Cloud LLC.

//! On-disk rule store under `/var/fw`. Ports `src/fw/lib/fw.js`
//! (loadAllRules/loadRule/saveRules/deleteRules) and `src/fw/lib/locker.js`
//! (flock-based shared/exclusive locks via libc::flock).

use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};

use serde_json::Value;

use crate::error::FwError;
use crate::model::{self, FwRule, RuleData};
use crate::parse::ParseOpts;

/// Base firewall directory.
pub const FW_DIR: &str = "/var/fw";
/// Rule files directory.
pub const RULE_PATH: &str = "/var/fw/rules";
/// flock target.
pub const FW_LOCK_FILE: &str = "/var/fw/.lockfile";
/// Default-rules bootstrap marker.
pub const DEFAULT_RULES_SETUP: &str = "/var/fw/.default_rules_setup";

const FW_DIR_MODE: u32 = 0o755;
const FW_FILE_MODE: u32 = 0o644;

/// A held flock; releases (flock un + close) on drop.
pub struct Lock {
    file: File,
}

impl Drop for Lock {
    fn drop(&mut self) {
        // Best-effort unlock; the fd close also drops the lock.
        unsafe {
            libc::flock(self.file.as_raw_fd(), libc::LOCK_UN);
        }
    }
}

/// A store rooted at a base directory (defaults to /var/fw). Parameterizing the
/// root lets verification run read-only against a copied store.
pub struct Store {
    base: PathBuf,
    rules: PathBuf,
    lockfile: PathBuf,
}

impl Default for Store {
    fn default() -> Self {
        Store::new(FW_DIR)
    }
}

impl Store {
    /// Create a store rooted at `base`.
    pub fn new(base: impl AsRef<Path>) -> Self {
        let base = base.as_ref().to_path_buf();
        let rules = base.join("rules");
        let lockfile = base.join(".lockfile");
        Store { base, rules, lockfile }
    }

    /// The rules directory path.
    pub fn rules_dir(&self) -> &Path {
        &self.rules
    }

    /// Open + lock the lockfile (tryOpen + flock). `exclusive` chooses ex vs sh.
    pub fn lock(&self, exclusive: bool) -> Result<Lock, FwError> {
        let file = self.try_open()?;
        let op = if exclusive { libc::LOCK_EX } else { libc::LOCK_SH };
        let rc = unsafe { libc::flock(file.as_raw_fd(), op) };
        if rc != 0 {
            return Err(FwError::from(std::io::Error::last_os_error()));
        }
        Ok(Lock { file })
    }

    /// Open the lockfile 'w+'; create /var/fw on ENOENT (locker.tryOpen).
    fn try_open(&self) -> Result<File, FwError> {
        match OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(FW_FILE_MODE)
            .open(&self.lockfile)
        {
            Ok(f) => Ok(f),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                fs::create_dir_all(&self.base)?;
                set_mode(&self.base, FW_DIR_MODE);
                Ok(OpenOptions::new()
                    .read(true)
                    .write(true)
                    .create(true)
                    .truncate(false)
                    .mode(FW_FILE_MODE)
                    .open(&self.lockfile)?)
            }
            Err(e) => Err(e.into()),
        }
    }

    /// Load all rules (loadAllRules). ENOENT on the rules dir -> empty. Files
    /// that fail to parse are skipped (logged in node; silently skipped here).
    pub fn load_all_rules(&self, opts: ParseOpts) -> Result<Vec<FwRule>, FwError> {
        let entries = match fs::read_dir(&self.rules) {
            Ok(e) => e,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Ok(Vec::new());
            }
            Err(e) => return Err(e.into()),
        };

        let mut rules = Vec::new();
        for entry in entries {
            let entry = entry?;
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if !name.ends_with(".json") {
                continue;
            }
            let path = entry.path();
            if let Ok(rule) = self.load_rule_file(&path, opts) {
                rules.push(rule);
            }
        }
        Ok(rules)
    }

    /// Load a single rule by uuid (loadRule). ENOENT -> Unknown rule error.
    pub fn load_rule(
        &self,
        uuid: &str,
        opts: ParseOpts,
    ) -> Result<FwRule, FwError> {
        let path = self.rules.join(format!("{uuid}.json"));
        match self.load_rule_file(&path, opts) {
            Ok(r) => Ok(r),
            Err(e) if e.code.as_deref() == Some("ENOENT") => Err(
                FwError::generic(format!("Unknown rule \"{uuid}\""))
                    .with_code("ENOENT"),
            ),
            Err(e) => Err(e),
        }
    }

    fn load_rule_file(
        &self,
        path: &Path,
        opts: ParseOpts,
    ) -> Result<FwRule, FwError> {
        let mut s = String::new();
        File::open(path)?.read_to_string(&mut s)?;
        let v: Value = serde_json::from_str(&s)?;
        rule_from_value(&v, opts)
    }

    /// Atomic save (saveRules): write each rule to a temp `<uuid>.json.<ver>`
    /// then rename over `<uuid>.json`.
    pub fn save_rules(&self, rules: &[FwRule]) -> Result<(), FwError> {
        fs::create_dir_all(&self.rules)?;
        set_mode(&self.rules, FW_DIR_MODE);

        let ver = format!(
            "{}.{:06}",
            chrono::Utc::now().timestamp_millis(),
            std::process::id()
        );

        let mut temps: Vec<(PathBuf, PathBuf)> = Vec::new();
        for rule in rules {
            let final_path = self.rules.join(format!("{}.json", rule.uuid));
            let temp_path =
                self.rules.join(format!("{}.json.{}", rule.uuid, ver));
            let json = serde_json::to_string_pretty(&rule.serialize())?;
            let mut f = OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(true)
                .mode(FW_FILE_MODE)
                .open(&temp_path)?;
            f.write_all(json.as_bytes())?;
            f.write_all(b"\n")?;
            temps.push((temp_path, final_path));
        }

        for (temp, final_path) in temps {
            fs::rename(&temp, &final_path)?;
        }
        Ok(())
    }

    /// Delete rules by uuid (deleteRules); ENOENT is success.
    pub fn delete_rules(&self, uuids: &[String]) -> Result<(), FwError> {
        for uuid in uuids {
            let path = self.rules.join(format!("{uuid}.json"));
            match fs::remove_file(&path) {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(e.into()),
            }
        }
        Ok(())
    }
}

/// Construct a FwRule from a serialized JSON object (as stored on disk).
pub fn rule_from_value(v: &Value, opts: ParseOpts) -> Result<FwRule, FwError> {
    let get_str = |k: &str| v.get(k).and_then(|x| x.as_str()).map(String::from);
    let get_bool = |k: &str| v.get(k).and_then(|x| x.as_bool());

    let data = RuleData {
        rule: get_str("rule"),
        parsed: None,
        uuid: get_str("uuid"),
        version: get_str("version"),
        owner_uuid: get_str("owner_uuid"),
        enabled: get_bool("enabled"),
        log: get_bool("log"),
        created_by: get_str("created_by"),
        description: get_str("description"),
    };
    FwRule::create(data, opts)
}

fn set_mode(path: &Path, mode: u32) {
    use std::os::unix::fs::PermissionsExt;
    if let Ok(meta) = fs::metadata(path) {
        let mut perm = meta.permissions();
        perm.set_mode(mode);
        let _ = fs::set_permissions(path, perm);
    }
}

/// Read the default-rules bootstrap marker, if present.
pub fn default_rules_marker(base: &Path) -> Option<String> {
    fs::read_to_string(base.join(".default_rules_setup"))
        .ok()
        .map(|s| s.trim().to_string())
}

/// Generate a fresh version string (re-exported for the CLI add path).
pub fn generate_version() -> String {
    model::generate_version()
}
