// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.
//
// Copyright 2026 Edgecast Cloud LLC.

//! `zonecfg` command-stream generation. The byte stream this produces is
//! piped to `zonecfg -z <zone>` on stdin, reproducing node `VM.js`.

pub mod build;
pub mod doc;

pub use build::build_create_zonecfg;
pub use doc::Zonecfg;
