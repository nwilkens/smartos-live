// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.
//
// Copyright 2026 Edgecast Cloud LLC.

//! Per-zone metadata transports.
//!
//! OS zones: bind a unix-domain listener at
//! `/var/zonecontrol/<uuid>/metadata.sock` and run the line protocol per
//! connection (the agent is the SERVER). HVM (bhyve/kvm): connect to the
//! hypervisor's com2 serial socket at `<zonepath>/root/tmp/vm.ttyb` and run
//! the same protocol over that single stream (the agent is the CLIENT). Both
//! share [`crate::handler`].

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{UnixListener, UnixStream};
use tracing::{debug, error, info, warn};

use crate::cache::VmCache;
use crate::handler::{self, LineBuffer, MetadataWriter};

/// node KVM_CONNECT_RETRY_INTERVAL.
const KVM_CONNECT_RETRY: Duration = Duration::from_millis(100);

/// The default OS-zone socket directory (`/var/zonecontrol`).
pub const ZONECONTROL_DIR: &str = "/var/zonecontrol";

/// Compute the OS-zone socket path for a uuid under `base`
/// (`<base>/<uuid>/metadata.sock`).
#[must_use]
pub fn os_socket_path(base: &Path, uuid: &str) -> PathBuf {
    base.join(uuid).join("metadata.sock")
}

/// Compute the HVM serial socket path (`<zonepath>/root/tmp/vm.ttyb`).
#[must_use]
pub fn hvm_socket_path(zonepath: &str) -> PathBuf {
    Path::new(zonepath).join("root/tmp/vm.ttyb")
}

/// Serve one OS-zone unix socket until cancelled. Binds `sock_path`, accepting
/// connections and dispatching each line against the live VM object for
/// `zonename`.
pub async fn serve_os_zone<W>(
    sock_path: PathBuf,
    zonename: String,
    cache: VmCache,
    writer: Arc<W>,
    mut shutdown: tokio::sync::watch::Receiver<bool>,
) where
    W: MetadataWriter + Send + Sync + 'static,
{
    // node unlinks a stale socket before binding.
    if sock_path.exists()
        && let Err(e) = std::fs::remove_file(&sock_path) {
            warn!("metadata: could not unlink stale {}: {e}", sock_path.display());
        }
    if let Some(parent) = sock_path.parent()
        && let Err(e) = std::fs::create_dir_all(parent) {
            error!("metadata: mkdir {} failed: {e}", parent.display());
            return;
        }

    let listener = match UnixListener::bind(&sock_path) {
        Ok(l) => l,
        Err(e) => {
            error!("metadata: bind {} failed: {e}", sock_path.display());
            return;
        }
    };
    info!("metadata: serving {} for zone {zonename}", sock_path.display());

    loop {
        tokio::select! {
            _ = shutdown.changed() => {
                if *shutdown.borrow() {
                    break;
                }
            }
            accepted = listener.accept() => {
                match accepted {
                    Ok((stream, _addr)) => {
                        let zn = zonename.clone();
                        let cache = cache.clone();
                        let writer = writer.clone();
                        tokio::spawn(async move {
                            handle_unix_conn(stream, zn, cache, writer).await;
                        });
                    }
                    Err(e) => {
                        warn!("metadata: accept on {} failed: {e}", sock_path.display());
                    }
                }
            }
        }
    }

    let _ = std::fs::remove_file(&sock_path);
    debug!("metadata: stopped serving {}", sock_path.display());
}

async fn handle_unix_conn<W>(mut stream: UnixStream, zonename: String, cache: VmCache, writer: Arc<W>)
where
    W: MetadataWriter + Send + Sync + 'static,
{
    let mut lb = LineBuffer::new();
    let mut buf = [0u8; 4096];
    loop {
        let n = match stream.read(&mut buf).await {
            Ok(0) => break,
            Ok(n) => n,
            Err(e) => {
                debug!("metadata: read error on zone {zonename}: {e}");
                break;
            }
        };
        for line in lb.push(&buf[..n]) {
            let Some(vm) = cache.get(&zonename) else {
                // node asserts the zone exists; we just drop if it vanished.
                warn!("metadata: request for unknown zone {zonename}");
                continue;
            };
            let resp = handler::handle_line(&line, &vm, &zonename, writer.as_ref());
            if !resp.is_empty()
                && let Err(e) = stream.write_all(&resp).await {
                    debug!("metadata: write error on zone {zonename}: {e}");
                    return;
                }
        }
    }
}

/// Connect to an HVM zone's com2 serial socket and run the protocol until the
/// stream closes (guest down/reboot). The caller re-invokes this on the next
/// boot. Retries connecting on ENOENT/ECONNREFUSED while not shut down.
pub async fn serve_hvm_zone<W>(
    sock_path: PathBuf,
    zonename: String,
    cache: VmCache,
    writer: Arc<W>,
    mut shutdown: tokio::sync::watch::Receiver<bool>,
) where
    W: MetadataWriter + Send + Sync + 'static,
{
    // Wait for the hypervisor to create + listen on the socket.
    let stream = loop {
        if *shutdown.borrow() {
            return;
        }
        match UnixStream::connect(&sock_path).await {
            Ok(s) => break s,
            Err(e) => {
                debug!("metadata: HVM connect {} retry: {e}", sock_path.display());
                tokio::select! {
                    _ = tokio::time::sleep(KVM_CONNECT_RETRY) => {}
                    _ = shutdown.changed() => {
                        if *shutdown.borrow() { return; }
                    }
                }
            }
        }
    };
    info!("metadata: connected HVM serial {} for zone {zonename}", sock_path.display());

    let mut stream = stream;
    let mut lb = LineBuffer::new();
    let mut buf = [0u8; 4096];
    loop {
        tokio::select! {
            _ = shutdown.changed() => {
                if *shutdown.borrow() { break; }
            }
            read = stream.read(&mut buf) => {
                match read {
                    Ok(0) => break, // guest closed (down/reboot)
                    Ok(n) => {
                        for line in lb.push(&buf[..n]) {
                            let Some(vm) = cache.get(&zonename) else { continue };
                            let resp = handler::handle_line(&line, &vm, &zonename, writer.as_ref());
                            if !resp.is_empty()
                                && let Err(e) = stream.write_all(&resp).await {
                                    debug!("metadata: HVM write error zone {zonename}: {e}");
                                    return;
                                }
                        }
                    }
                    Err(e) => {
                        debug!("metadata: HVM read error zone {zonename}: {e}");
                        break;
                    }
                }
            }
        }
    }
    debug!("metadata: HVM serial closed for zone {zonename}");
}

/// True for brands that use the serial (com2) metadata path.
#[must_use]
pub fn is_hvm_brand(brand: &str) -> bool {
    matches!(brand, "bhyve" | "kvm")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn os_socket_path_layout() {
        let p = os_socket_path(Path::new("/var/zonecontrol"), "abc");
        assert_eq!(p, PathBuf::from("/var/zonecontrol/abc/metadata.sock"));
    }

    #[test]
    fn hvm_socket_path_layout() {
        let p = hvm_socket_path("/zones/abc");
        assert_eq!(p, PathBuf::from("/zones/abc/root/tmp/vm.ttyb"));
    }

    #[test]
    fn brand_classification() {
        assert!(is_hvm_brand("bhyve"));
        assert!(is_hvm_brand("kvm"));
        assert!(!is_hvm_brand("joyent-minimal"));
        assert!(!is_hvm_brand("lx"));
    }
}
