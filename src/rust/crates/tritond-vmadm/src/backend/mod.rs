// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.
//
// Copyright 2026 Edgecast Cloud LLC.

//! The host abstraction the lifecycle code drives ([`traits`]), plus the
//! in-memory test doubles ([`fake`], test builds only).

pub mod host;
pub mod traits;

#[cfg(test)]
pub mod fake;

pub use host::{HostBackend, HostNicTagRegistrar};
pub use traits::{InstallArgs, NicTagRegistrar, ProvisionOutcome, ZoneHost, ZoneRef};
