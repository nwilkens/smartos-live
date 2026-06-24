// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.
//
// Copyright 2026 Edgecast Cloud LLC.

//! Normalized engine model: the brand-agnostic [`spec::VmCreateSpec`] the
//! create algorithm consumes, plus its NIC/disk components.

pub mod disk;
pub mod get_json;
pub mod nic;
pub mod spec;

pub use disk::{DiskMedia, DiskModel, DiskSpec, Refreservation};
pub use get_json::{DiskJson, NicJson, SnapshotJson, VmGetJson};
pub use nic::{NicAddr, NicSpec, fix_mac, ruin_mac};
pub use spec::{JsonMap, VmCreateSpec};
