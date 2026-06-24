// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.
//
// Copyright 2026 Edgecast Cloud LLC.

//! The engine error type. `rest_code()` mirrors the small set of error
//! codes node-vmadm callers branch on (matching
//! `tritond_cn_platform::smartos::vmadm::VmadmError::rest_code`), so the
//! drop-in binary and the in-process callers see the same semantics.

use std::path::PathBuf;
use std::process::ExitStatus;

use uuid::Uuid;

#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum VmadmError {
    #[error("VM {0} already exists")]
    AlreadyExists(Uuid),

    #[error("VM {0} not found")]
    NotFound(Uuid),

    #[error("VM {uuid} must be stopped (state={state})")]
    NotStopped { uuid: Uuid, state: String },

    #[error("VM {0} is not running")]
    NotRunning(Uuid),

    #[error("brand {0:?} is not supported by this vmadm")]
    UnsupportedBrand(String),

    #[error("provisioning dataset {image} with brand {brand} is not supported")]
    DatasetNotProvisionable { image: Uuid, brand: String },

    #[error("a disk model is required for bhyve disks")]
    DiskModelRequired,

    #[error("VM {0} is indestructible (a held @indestructible snapshot exists); refusing destructive operation")]
    Indestructible(Uuid),

    #[error("nic_tag {link:?} is not registered and the {brand} brand will not boot without it")]
    NicTagUnregistered { link: String, brand: String },

    #[error("invalid payload: {0}")]
    Payload(String),

    #[error("zonecfg exited {status}: {stderr}")]
    Zonecfg { status: ExitStatus, stderr: String },

    #[error("seed-disk build failed: {0}")]
    Seed(String),

    #[error("rollback left residue: {0}")]
    RollbackResidue(String),

    #[error("failed to spawn {bin}: {source}")]
    Spawn {
        bin: String,
        #[source]
        source: std::io::Error,
    },

    #[error("{bin} exited {status}: {stderr}")]
    NonZeroExit {
        bin: String,
        status: ExitStatus,
        stderr: String,
    },

    #[error("io {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error("zfs: {0}")]
    Zfs(#[from] tritond_cn_platform::smartos::zfs::ZfsError),

    #[error("zoneadm: {0}")]
    Zoneadm(#[from] tritond_cn_platform::smartos::zoneadm::ZoneadmError),
}

impl VmadmError {
    /// The REST-style error code a node-vmadm caller would branch on, for
    /// the subset of errors that have one. Parity with
    /// `tritond_cn_platform::smartos::vmadm::VmadmError::rest_code`.
    #[must_use]
    pub fn rest_code(&self) -> Option<&'static str> {
        match self {
            VmadmError::NotFound(_) => Some("VM_NOT_FOUND"),
            VmadmError::AlreadyExists(_) => Some("VM_ALREADY_EXISTS"),
            VmadmError::NotStopped { .. } => Some("VM_NOT_STOPPED"),
            VmadmError::NotRunning(_) => Some("VM_NOT_RUNNING"),
            _ => None,
        }
    }
}

pub type Result<T> = std::result::Result<T, VmadmError>;
