// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.
//
// Copyright 2026 Edgecast Cloud LLC.

//! The per-HVM remote-display (VNC/SPICE) and console TCP proxies.
//!
//! Each running HVM gets a raw-TCP listener bound to the host admin IP that
//! splices bytes to an in-zone unix socket: `<zonepath>/root/tmp/vm.vnc` for
//! VNC, `<zonepath>/root/tmp/vm.spice` for SPICE, and either
//! `<zonepath>/root/tmp/vm.console` (KVM) or
//! `/var/run/zones/<zonename>.console_sock` (bhyve/joyent-minimal, with the
//! zoneadmd `IDENT C 0` handshake) for the console. Listeners are tracked in
//! per-uuid registries (`VNC`/`SPICE`/`CONSOLE` in node) so `vmadm info` can
//! report `{host, port, display}` and so a reboot/reload can tear them down
//! and respawn them.
//!
//! This reproduces node `vmadmd.js` `spawnRemoteDisplay` (L262),
//! `spawnConsoleProxy` (L433), `clearRemoteDisplay` (L390),
//! `reloadRemoteDisplay` (L408), and the `infoVNC`/`infoConsole` reporting
//! (L1790/L1806). The byte-for-byte contract is the `display = port - 5900`
//! quirk and the present-when-set key emission.

use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;

use serde_json::{Map, Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream, UnixStream};

/// The kind of remote-display protocol fronted by a listener.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DisplayProtocol {
    Vnc,
    /// SPICE is KVM-only (P9). The selection branch and `SPICE` registry are
    /// carried so P9 only flips the brand gate; bhyve never sets spice_port>0.
    Spice,
}

/// A live listener entry: its advertised host/port plus a handle to stop it.
#[derive(Debug)]
struct Listener {
    host: IpAddr,
    port: u16,
    /// `port - 5900`, only set when `port >= 5900` (node's display quirk).
    display: Option<u16>,
    /// Optional QMP-pushed display password (vnc_password/spice_password).
    password: Option<String>,
    /// SPICE-only opts string (carried for P9).
    spice_opts: Option<String>,
    /// Console reports a `type` ("socket") and the backing path.
    console_type: Option<String>,
    /// Dropping this aborts the accept loop, which drops the `TcpListener`.
    _abort: tokio::task::AbortHandle,
}

/// The shared registries keyed by VM uuid. `Arc<Registry>` is cloned into the
/// HTTP handlers and the zone-event/autoboot paths.
#[derive(Default)]
pub struct Registry {
    vnc: Mutex<HashMap<String, Listener>>,
    spice: Mutex<HashMap<String, Listener>>,
    console: Mutex<HashMap<String, Listener>>,
}

/// Parameters needed to spawn a remote-display proxy for one VM, lifted from
/// the vminfod VM object the way node's `VM.load(fields)` does.
#[derive(Debug, Clone)]
pub struct DisplaySpec {
    pub uuid: String,
    pub zonepath: PathBuf,
    /// The brand, so the display proxy knows whether to push the password
    /// into the guest via QMP (kvm) before binding the listener.
    pub brand: String,
    pub state: String,
    pub zone_state: String,
    pub vnc_port: i64,
    pub vnc_password: Option<String>,
    pub spice_port: i64,
    pub spice_password: Option<String>,
    pub spice_opts: Option<String>,
}

/// Parameters needed to spawn a console proxy for one VM.
#[derive(Debug, Clone)]
pub struct ConsoleSpec {
    pub uuid: String,
    pub zonename: String,
    pub zonepath: PathBuf,
    pub brand: String,
    pub state: String,
    pub zone_state: String,
}

impl DisplaySpec {
    /// Build a display spec from a vminfod VM object, defaulting missing
    /// fields exactly as node's `spawnRemoteDisplay` does (vnc_port absent =>
    /// 0 => OS-assigned ephemeral port).
    #[must_use]
    pub fn from_vmobj(obj: &Value) -> Option<Self> {
        let uuid = obj.get("uuid")?.as_str()?.to_string();
        let zonepath = obj
            .get("zonepath")
            .and_then(Value::as_str)
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from(format!("/zones/{uuid}")));
        Some(Self {
            uuid,
            zonepath,
            brand: str_field(obj, "brand"),
            state: str_field(obj, "state"),
            zone_state: str_field(obj, "zone_state"),
            vnc_port: obj.get("vnc_port").and_then(Value::as_i64).unwrap_or(0),
            vnc_password: nonempty_str(obj, "vnc_password"),
            spice_port: obj.get("spice_port").and_then(Value::as_i64).unwrap_or(0),
            spice_password: nonempty_str(obj, "spice_password"),
            spice_opts: nonempty_str(obj, "spice_opts"),
        })
    }
}

impl ConsoleSpec {
    #[must_use]
    pub fn from_vmobj(obj: &Value) -> Option<Self> {
        let uuid = obj.get("uuid")?.as_str()?.to_string();
        let zonename = obj
            .get("zonename")
            .and_then(Value::as_str)
            .map(str::to_string)
            .unwrap_or_else(|| uuid.clone());
        let zonepath = obj
            .get("zonepath")
            .and_then(Value::as_str)
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from(format!("/zones/{uuid}")));
        Some(Self {
            uuid,
            zonename,
            zonepath,
            brand: str_field(obj, "brand"),
            state: str_field(obj, "state"),
            zone_state: str_field(obj, "zone_state"),
        })
    }
}

fn str_field(obj: &Value, key: &str) -> String {
    obj.get(key)
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string()
}

fn nonempty_str(obj: &Value, key: &str) -> Option<String> {
    obj.get(key)
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

impl Registry {
    #[must_use]
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// node `spawnRemoteDisplay` (vmadmd.js:262). Selects SPICE when
    /// `spice_port > 0`, else VNC. Skips non-running VMs and a `port == -1`
    /// disabled listener. Binds the chosen port (0 = ephemeral) on `admin_ip`,
    /// reads back the actual port, and records the registry entry.
    pub async fn spawn_remote_display(&self, spec: &DisplaySpec, admin_ip: IpAddr) {
        let (protocol, port, sockpath, password, spice_opts) = if spec.spice_port > 0 {
            // P9: SPICE is KVM-only. Carried so the selection branch exists.
            (
                DisplayProtocol::Spice,
                spec.spice_port,
                spec.zonepath.join("root/tmp/vm.spice"),
                spec.spice_password.clone(),
                spec.spice_opts.clone(),
            )
        } else {
            (
                DisplayProtocol::Vnc,
                spec.vnc_port,
                spec.zonepath.join("root/tmp/vm.vnc"),
                spec.vnc_password.clone(),
                None,
            )
        };

        if spec.state != "running" && spec.zone_state != "running" {
            tracing::debug!(uuid = %spec.uuid, "skipping display setup for non-running VM");
            return;
        }
        if port == -1 {
            tracing::info!(uuid = %spec.uuid, "display listener disabled (port == -1)");
            return;
        }

        // For kvm, push the display password into the guest via QMP
        // `set_password` BEFORE binding the listener (setRemoteDisplayPassword,
        // vmadmd.js:218/334). qemu enforces the password on the in-zone vm.vnc/
        // vm.spice socket; bhyve has no QMP monitor, so its password is only
        // surfaced in `vmadm info` (the in-zone socket does not enforce it).
        let proto_str = if protocol == DisplayProtocol::Vnc { "vnc" } else { "spice" };
        if spec.brand == "kvm"
            && let Some(pw) = &password
            && !pw.is_empty()
        {
            set_remote_display_password(&spec.zonepath, proto_str, pw).await;
        }

        let bind_port = u16::try_from(port).unwrap_or(0);
        let bind_addr = SocketAddr::new(admin_ip, bind_port);
        let listener = match TcpListener::bind(bind_addr).await {
            Ok(l) => l,
            Err(e) => {
                tracing::error!(uuid = %spec.uuid, %bind_addr, error = %e,
                    "failed to bind display listener");
                return;
            }
        };
        let actual = match listener.local_addr() {
            Ok(a) => a,
            Err(e) => {
                tracing::error!(uuid = %spec.uuid, error = %e, "no local_addr for display");
                return;
            }
        };

        let sock = sockpath.clone();
        let uuid = spec.uuid.clone();
        let proto_name = proto_str;
        let task = tokio::spawn(accept_loop(listener, sock, uuid.clone(), proto_name));

        let display = if actual.port() >= 5900 {
            Some(actual.port() - 5900)
        } else {
            None
        };
        let entry = Listener {
            host: admin_ip,
            port: actual.port(),
            display,
            password,
            spice_opts,
            console_type: None,
            _abort: task.abort_handle(),
        };
        tracing::info!(uuid = %spec.uuid, protocol = proto_name, host = %admin_ip,
            port = actual.port(), "spawned display listener");
        match protocol {
            DisplayProtocol::Vnc => {
                self.vnc.lock().map(|mut m| m.insert(uuid, entry)).ok();
            }
            DisplayProtocol::Spice => {
                self.spice.lock().map(|mut m| m.insert(uuid, entry)).ok();
            }
        }
    }

    /// node `spawnConsoleProxy` (vmadmd.js:433). KVM proxies to
    /// `<zonepath>/root/tmp/vm.console` with no handshake; other brands proxy
    /// to `/var/run/zones/<zonename>.console_sock` with the zoneadmd
    /// `IDENT C 0` -> `OK` handshake (5s timeout). Always ephemeral port.
    pub async fn spawn_console_proxy(&self, spec: &ConsoleSpec, admin_ip: IpAddr) {
        if spec.state != "running" && spec.zone_state != "running" {
            tracing::debug!(uuid = %spec.uuid, "skipping console setup for non-running VM");
            return;
        }
        let (console_path, needs_handshake) = if spec.brand == "kvm" {
            (spec.zonepath.join("root/tmp/vm.console"), false)
        } else {
            (
                PathBuf::from(format!("/var/run/zones/{}.console_sock", spec.zonename)),
                true,
            )
        };

        let bind_addr = SocketAddr::new(admin_ip, 0);
        let listener = match TcpListener::bind(bind_addr).await {
            Ok(l) => l,
            Err(e) => {
                tracing::error!(uuid = %spec.uuid, %bind_addr, error = %e,
                    "failed to bind console listener");
                return;
            }
        };
        let actual = match listener.local_addr() {
            Ok(a) => a,
            Err(e) => {
                tracing::error!(uuid = %spec.uuid, error = %e, "no local_addr for console");
                return;
            }
        };

        let uuid = spec.uuid.clone();
        let path = console_path.clone();
        let task = tokio::spawn(console_accept_loop(
            listener,
            path,
            uuid.clone(),
            needs_handshake,
        ));

        let entry = Listener {
            host: admin_ip,
            port: actual.port(),
            display: None,
            password: None,
            spice_opts: None,
            console_type: Some("socket".to_string()),
            _abort: task.abort_handle(),
        };
        tracing::info!(uuid = %spec.uuid, host = %admin_ip, port = actual.port(),
            brand = %spec.brand, "spawned console listener");
        self.console.lock().map(|mut m| m.insert(uuid, entry)).ok();
    }

    /// node `clearRemoteDisplay` (vmadmd.js:390): close + drop both the VNC
    /// and SPICE listeners for a uuid.
    pub fn clear_remote_display(&self, uuid: &str) {
        self.vnc.lock().map(|mut m| m.remove(uuid)).ok();
        self.spice.lock().map(|mut m| m.remove(uuid)).ok();
    }

    /// node `clearConsoleProxy` (vmadmd.js:579).
    pub fn clear_console_proxy(&self, uuid: &str) {
        self.console.lock().map(|mut m| m.remove(uuid)).ok();
    }

    /// node `clearVM` (vmadmd.js:603): clears display + console (the stop
    /// timer is tracked separately in [`crate::timer`]).
    pub fn clear_vm(&self, uuid: &str) {
        self.clear_remote_display(uuid);
        self.clear_console_proxy(uuid);
    }

    /// node `reloadRemoteDisplay` (vmadmd.js:408): clear then respawn.
    pub async fn reload_remote_display(&self, spec: &DisplaySpec, admin_ip: IpAddr) {
        self.clear_remote_display(&spec.uuid);
        self.spawn_remote_display(spec, admin_ip).await;
    }

    /// node `infoVNC` (vmadmd.js:1790): `{host, port[, display][, password]}`,
    /// each key only present when set. Returns `{}` if no listener.
    #[must_use]
    pub fn info_vnc(&self, uuid: &str) -> Value {
        let mut out = Map::new();
        if let Ok(m) = self.vnc.lock()
            && let Some(l) = m.get(uuid)
        {
            out.insert("host".into(), json!(l.host.to_string()));
            out.insert("port".into(), json!(l.port));
            if let Some(d) = l.display {
                out.insert("display".into(), json!(d));
            }
            if let Some(p) = &l.password {
                out.insert("password".into(), json!(p));
            }
        }
        Value::Object(out)
    }

    /// node SPICE info branch (vmadmd.js:1737): `{host, port[, password]
    /// [, spice_opts]}` (no display). KVM-only in practice (P9).
    #[must_use]
    pub fn info_spice(&self, uuid: &str) -> Value {
        let mut out = Map::new();
        if let Ok(m) = self.spice.lock()
            && let Some(l) = m.get(uuid)
        {
            out.insert("host".into(), json!(l.host.to_string()));
            out.insert("port".into(), json!(l.port));
            if let Some(p) = &l.password {
                out.insert("password".into(), json!(p));
            }
            if let Some(o) = &l.spice_opts {
                out.insert("spice_opts".into(), json!(o));
            }
        }
        Value::Object(out)
    }

    /// node `infoConsole` (vmadmd.js:1806): `{host, port, type}`.
    #[must_use]
    pub fn info_console(&self, uuid: &str) -> Value {
        let mut out = Map::new();
        if let Ok(m) = self.console.lock()
            && let Some(l) = m.get(uuid)
        {
            out.insert("host".into(), json!(l.host.to_string()));
            out.insert("port".into(), json!(l.port));
            if let Some(t) = &l.console_type {
                out.insert("type".into(), json!(t));
            }
        }
        Value::Object(out)
    }

    /// Test/diagnostic: the advertised VNC port for a uuid, if any.
    #[must_use]
    pub fn vnc_port(&self, uuid: &str) -> Option<u16> {
        self.vnc.lock().ok().and_then(|m| m.get(uuid).map(|l| l.port))
    }
}

/// Accept inbound TCP clients and splice each to a fresh connection to the
/// in-zone display unix socket. node pipes `c <-> dpy` bidirectionally.
async fn accept_loop(listener: TcpListener, sockpath: PathBuf, uuid: String, protocol: &str) {
    loop {
        let (client, peer) = match listener.accept().await {
            Ok(pair) => pair,
            Err(e) => {
                tracing::warn!(uuid = %uuid, error = %e, "display accept error");
                continue;
            }
        };
        let sock = sockpath.clone();
        let uuid_c = uuid.clone();
        let proto = protocol.to_string();
        tokio::spawn(async move {
            tracing::info!(uuid = %uuid_c, protocol = %proto, %peer, "display connection started");
            match UnixStream::connect(&sock).await {
                Ok(dpy) => {
                    splice(client, dpy).await;
                }
                Err(e) => {
                    tracing::warn!(uuid = %uuid_c, error = %e, sock = %sock.display(),
                        "display socket connect error");
                }
            }
            tracing::info!(uuid = %uuid_c, protocol = %proto, %peer, "display connection ended");
        });
    }
}

/// Console accept loop. For non-KVM brands, perform the zoneadmd zlogin
/// handshake (`IDENT C 0\n` -> response starting with `OK`, 5s timeout) before
/// splicing. node `spawnConsoleProxy` inner server (vmadmd.js:460).
async fn console_accept_loop(
    listener: TcpListener,
    console_path: PathBuf,
    uuid: String,
    needs_handshake: bool,
) {
    loop {
        let (client, peer) = match listener.accept().await {
            Ok(pair) => pair,
            Err(e) => {
                tracing::warn!(uuid = %uuid, error = %e, "console accept error");
                continue;
            }
        };
        let path = console_path.clone();
        let uuid_c = uuid.clone();
        tokio::spawn(async move {
            tracing::info!(uuid = %uuid_c, %peer, "console connection started");
            let mut cons = match UnixStream::connect(&path).await {
                Ok(s) => s,
                Err(e) => {
                    tracing::warn!(uuid = %uuid_c, error = %e, path = %path.display(),
                        "console socket connect error");
                    return;
                }
            };
            if needs_handshake {
                match console_handshake(&mut cons).await {
                    Ok(()) => {}
                    Err(e) => {
                        tracing::error!(uuid = %uuid_c, error = %e, "console handshake failed");
                        return;
                    }
                }
            }
            splice(client, cons).await;
            tracing::info!(uuid = %uuid_c, %peer, "console connection ended");
        });
    }
}

/// Send the zoneadmd zlogin handshake and wait for an `OK`-prefixed reply,
/// bounded by a 5s timeout (node's handshakeTimer).
async fn console_handshake(cons: &mut UnixStream) -> Result<(), String> {
    cons.write_all(b"IDENT C 0\n")
        .await
        .map_err(|e| e.to_string())?;
    let mut buf = [0u8; 256];
    let n = tokio::time::timeout(Duration::from_secs(5), cons.read(&mut buf))
        .await
        .map_err(|_| "handshake timeout".to_string())?
        .map_err(|e| e.to_string())?;
    let reply = String::from_utf8_lossy(&buf[..n]);
    if reply.starts_with("OK") {
        Ok(())
    } else {
        Err(format!("unexpected handshake reply: {}", &reply[..reply.len().min(100)]))
    }
}

/// node `setRemoteDisplayPassword` (vmadmd.js:218): connect the guest's QMP
/// monitor and send `set_password {protocol, password}`. Best-effort — node
/// only logs a warning on failure (the listener still binds), so a QMP error
/// here does not abort the proxy spawn.
async fn set_remote_display_password(zonepath: &Path, protocol: &str, password: &str) {
    let socket = crate::qmp::qmp_socket_path(zonepath);
    let mut qmp = match crate::qmp::Qmp::connect(&socket).await {
        Ok(q) => q,
        Err(e) => {
            tracing::warn!(protocol, error = %e, "qmp connect for set_password failed");
            return;
        }
    };
    let args = json!({ "protocol": protocol, "password": password });
    match qmp.command("set_password", Some(args)).await {
        Ok(_) => tracing::debug!(protocol, "set display password via QMP"),
        Err(e) => tracing::warn!(protocol, error = %e, "QMP set_password failed"),
    }
    qmp.disconnect().await;
}

/// Bidirectionally copy bytes between a TCP client and a unix-socket display
/// endpoint until either side closes (tokio's `copy_bidirectional`).
async fn splice(client: TcpStream, server: UnixStream) {
    let (mut cr, mut cw) = client.into_split();
    let (mut sr, mut sw) = server.into_split();
    let c2s = async {
        let _ = tokio::io::copy(&mut cr, &mut sw).await;
        let _ = sw.shutdown().await;
    };
    let s2c = async {
        let _ = tokio::io::copy(&mut sr, &mut cw).await;
        let _ = cw.shutdown().await;
    };
    tokio::join!(c2s, s2c);
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn display_spec_defaults_vnc_port_to_zero() {
        let obj = json!({
            "uuid": "b8a9f468-b364-44e0-927a-b4c799c915f1",
            "state": "running",
            "zone_state": "running",
            "brand": "bhyve"
        });
        let spec = DisplaySpec::from_vmobj(&obj).expect("spec");
        assert_eq!(spec.vnc_port, 0);
        assert_eq!(spec.spice_port, 0);
        assert_eq!(
            spec.zonepath,
            PathBuf::from("/zones/b8a9f468-b364-44e0-927a-b4c799c915f1")
        );
    }

    #[test]
    fn info_vnc_empty_when_no_listener() {
        let reg = Registry::default();
        assert_eq!(reg.info_vnc("nope"), json!({}));
    }

    #[test]
    fn display_quirk_5900_offset() {
        // The display = port - 5900 quirk only applies for port >= 5900.
        let reg = Registry::default();
        let (_t, abort) = abort_pair();
        reg.vnc.lock().unwrap().insert(
            "u".into(),
            Listener {
                host: "10.0.0.1".parse().unwrap(),
                port: 64428,
                display: Some(64428 - 5900),
                password: None,
                spice_opts: None,
                console_type: None,
                _abort: abort,
            },
        );
        let info = reg.info_vnc("u");
        assert_eq!(info["port"], json!(64428));
        assert_eq!(info["display"], json!(58528));
        assert_eq!(info["host"], json!("10.0.0.1"));
        assert!(info.get("password").is_none());
    }

    #[test]
    fn info_console_shape() {
        let reg = Registry::default();
        let (_t, abort) = abort_pair();
        reg.console.lock().unwrap().insert(
            "u".into(),
            Listener {
                host: "10.0.0.1".parse().unwrap(),
                port: 12345,
                display: None,
                password: None,
                spice_opts: None,
                console_type: Some("socket".into()),
                _abort: abort,
            },
        );
        let info = reg.info_console("u");
        assert_eq!(info["type"], json!("socket"));
        assert_eq!(info["port"], json!(12345));
    }

    /// Build a parked task we can take an `AbortHandle` from for tests.
    fn abort_pair() -> (tokio::runtime::Runtime, tokio::task::AbortHandle) {
        let rt = tokio::runtime::Builder::new_current_thread()
            .build()
            .expect("rt");
        let handle = rt.spawn(async {
            // Never resolves; tests only need the abort handle.
            std::future::pending::<()>().await;
        });
        let abort = handle.abort_handle();
        (rt, abort)
    }
}
