// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.
//
// Copyright 2026 Edgecast Cloud LLC.

//! `fwadm` library: a faithful Rust port of the SmartOS node fwadm.
//!
//! Modules:
//! - [`parse`]: the fwrule DSL parser (jison grammar port).
//! - [`model`]: the FwRule object model + canonical serializer.
//! - [`store`]: the `/var/fw/rules` JSON store with flock locking.
//! - [`ipf`]: the rules + VM -> per-zone ipf.conf compiler.
//! - [`apply`]: the ipf/ipfstat subprocess apply layer.
//! - [`vm`]: the VM model + lookup for target resolution.

pub mod apply;
pub mod error;
pub mod ipf;
pub mod model;
pub mod parse;
pub mod store;
pub mod vm;
pub mod vmsource;

pub use error::FwError;
