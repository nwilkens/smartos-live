// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.
//
// Copyright 2026 Edgecast Cloud LLC.

//! VM lifecycle orchestrators. `create` lands first (joyent-minimal);
//! start/stop/reboot/delete/update/reprovision follow in later slices.

pub mod control;
pub mod create;
pub mod get;
pub mod holds;
pub mod reprovision;
pub mod update;

pub use control::{delete, delete_in, reboot, start, stop};
pub use get::{get, list};
pub use reprovision::reprovision;
pub use update::{grow_boot_disk, grow_boot_disk_in};
