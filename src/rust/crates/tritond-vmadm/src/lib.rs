// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.
//
// Copyright 2026 Edgecast Cloud LLC.

//! `tritond-vmadm` — a native Rust replacement for the SmartOS node.js
//! `vmadm`, scoped to Triton vNext's two brands (`bhyve` and
//! `joyent-minimal`). It builds zone configuration and drives
//! `zonecfg` / `zoneadm` / `zfs` directly, removing the node runtime from
//! a compute node's VM-management path.
//!
//! The single create input is the node-vmadm-compatible JSON payload
//! ([`payload::VmadmCreatePayload`]); see `DESIGN.md` for the full
//! architecture and the source-grounded brand mechanics it reproduces.

pub mod backend;
pub mod brand;
pub mod constants;
pub mod error;
pub mod files;
pub mod journal;
pub mod lifecycle;
pub mod lock;
pub mod model;
pub mod payload;
pub mod quota;
pub mod seed;
pub mod zonecfg;

pub use backend::{HostBackend, HostNicTagRegistrar, NicTagRegistrar, ZoneHost};
pub use brand::Brand;
pub use error::{Result, VmadmError};
pub use lifecycle::create::{create, create_in};
pub use model::{VmCreateSpec, VmGetJson};
pub use payload::VmadmCreatePayload;
pub use zonecfg::build_create_zonecfg;
