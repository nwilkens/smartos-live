// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.
//
// Copyright 2026 Edgecast Cloud LLC.

//! Native Rust replacement for the SmartOS node metadata agent.
//!
//! Serves the mdata V1/V2 line protocol on per-zone unix sockets (OS zones)
//! and the bhyve/kvm com2 serial socket (HVM), sourcing VM objects from
//! vminfod. The protocol (`crate::protocol`), CRC32 framing (`crate::crc32`),
//! and key resolution (`crate::resolver`) are pure and unit-tested against
//! live wire goldens; the daemon (`crate::daemon`) wires them to the cache and
//! transports.

pub mod cache;
pub mod crc32;
pub mod daemon;
pub mod handler;
pub mod protocol;
pub mod resolver;
pub mod server;
pub mod vminfo_client;
pub mod writer;

pub use daemon::{Agent, Config};
pub use handler::{MetadataWriter, ReadOnlyWriter};
pub use resolver::WriteOp;
pub use writer::FileWriter;
