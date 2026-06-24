// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.
//
// Copyright 2026 Edgecast Cloud LLC.

//! `vminfod` — a native Rust replacement for the SmartOS node vminfod
//! daemon.
//!
//! It serves the same HTTP interface on `127.0.0.1:9090`
//! (`/ping` `/status` `/vms` `/vms/:uuid` `/events`) over an in-memory VM
//! cache. VM objects are produced by [`tritond_vmadm`] (the vmload-equivalent
//! reconstruction) and reordered into vminfod's exact key insertion order
//! ([`vmobj`]). The cache is seeded by a full [`loader`] refresh and kept
//! current by a zone state-change sysevent [`watcher`] plus a periodic full
//! refresh. `/events` streams newline-delimited JSON ([`events`]) so VM.js,
//! the metadata agent's `VminfodWatcher`, vmadmd's `ZoneEvent`, and the
//! `zoneevent` CLI keep working unmodified.

pub mod cache;
pub mod datasets;
pub mod diff;
pub mod events;
pub mod fswatcher;
pub mod hrtime;
pub mod loader;
pub mod server;
pub mod state;
pub mod vmobj;
pub mod watcher;

pub use state::Daemon;
