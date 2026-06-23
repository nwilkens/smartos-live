// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.
//
// Copyright 2026 Edgecast Cloud LLC.

//! The engine's create input: the node-vmadm-compatible JSON payload
//! ([`compat`]) and its normalization into a [`crate::model::VmCreateSpec`]
//! ([`normalize`]).

pub mod compat;
pub mod normalize;

pub use compat::{DiskPayload, NicPayload, VmadmCreatePayload};
pub use normalize::normalize;
