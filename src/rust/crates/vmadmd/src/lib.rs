// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.
//
// Copyright 2026 Edgecast Cloud LLC.

//! `vmadmd` — a native Rust replacement for the SmartOS node `vmadmd`
//! control daemon, scoped to the bhyve + joyent-minimal brands (P6).
//!
//! vmadmd is an SMF service that runs a CUSTOM (non-REST, non-vminfod) HTTP
//! control plane on `127.0.0.1:8080`. Its only client is VM.js. This crate
//! reproduces the byte-compatible contract:
//!
//! * [`http`] — the control-plane router (`GET /vm/:uuid/info`,
//!   `POST /vm/:uuid?action=...`);
//! * [`proxy`] — the per-HVM VNC/SPICE/console TCP proxy bound to the host
//!   admin IP, with the `display = port - 5900` info quirk;
//! * [`state`] — the shared daemon state and the control-verb logic;
//! * [`responsibilities`] — autoboot, provisioning-marker handling, and KVM
//!   log rotation;
//! * [`vminfo_client`] — the async vminfod client that replaces `VM.load` /
//!   `VM.lookup`.
//!
//! KVM and SPICE are deferred to P9; the selection branches and registries
//! are carried with `TODO(P9-KVM)` markers so the brand path slots in.

pub mod http;
pub mod proxy;
pub mod responsibilities;
pub mod state;
pub mod vminfo_client;

/// node `VMADMD_PORT` (vmadmd.js:63). The control plane binds loopback only.
pub const VMADMD_PORT: u16 = 8080;

/// The loopback bind address node hardcodes (`ips = ['127.0.0.1']`).
pub const VMADMD_BIND: &str = "127.0.0.1";

pub use proxy::Registry;
pub use state::Daemon;
pub use vminfo_client::VminfoClient;
