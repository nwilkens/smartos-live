// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.
//
// Copyright 2026 Edgecast Cloud LLC.

//! The QMP (QEMU Machine Protocol) client used to control a running kvm
//! guest over its in-zone unix monitor socket (`<zonepath>/root/tmp/vm.qmp`,
//! which the guest sees as `/tmp/vm.qmp`).
//!
//! This is the Rust port of node `qmp.js` (the `Qmp` class) plus the
//! connect-per-command usage in `vmadmd.js` (stopVM:1603, resetVM:1851,
//! sysrqVM:1905, infoVM:1695, setRemoteDisplayPassword:225). Like node, each
//! control verb opens a fresh connection, performs the handshake, issues its
//! command(s), and disconnects — there is no persistent per-VM connection
//! and no async-event consumption.
//!
//! Wire protocol (qmp.js:105-195):
//! * Transport: a UNIX stream to the monitor socket (qemu's `-qmp
//!   chardev:...,server`). Encoding utf8, framing newline-delimited JSON.
//! * Inbound shapes distinguished by key: `{"QMP":{...}}` greeting (logged,
//!   ignored); `{"return":<v>,"id":<n>}` success matched by id;
//!   `{"event":...,"timestamp":{seconds,microseconds}}` async event (logged);
//!   anything else / `{"error":{"class","desc"},"id":<n>}` an error.
//! * Handshake (qmp.js:152): after connect, send `qmp_capabilities` exactly
//!   once; only after its `return` may other commands be issued.
//! * Command framing (qmp.js:175): `{"execute":<cmd>,"id":<int>[,"arguments":
//!   <args>]}` + '\n'; id is a monotonic integer; responses are matched by id.
//!
//! Two deliberate hardening deltas from node (per the P9 spec): node has NO
//! `{"error":...}` branch — a failed command silently never resolves — so we
//! add an error-reply branch AND a connect/handshake/command timeout
//! (mirroring the P6 console handshake timeout) so a wedged guest fails fast
//! instead of hanging the daemon.

use std::path::Path;
use std::time::Duration;

use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use tokio::net::unix::OwnedReadHalf;
use tokio::net::unix::OwnedWriteHalf;

/// Connect + handshake timeout, and the per-command response timeout. node
/// has none (a wedged guest hangs forever); we bound it like the P6 console
/// handshake (5s) to keep the control plane responsive.
const QMP_TIMEOUT: Duration = Duration::from_secs(5);

/// A QMP failure. The variants map to the distinct ways a control verb can
/// fail so the caller can surface node-compatible error text.
#[derive(Debug)]
pub enum QmpError {
    /// The monitor socket could not be opened (guest not running / no qemu).
    Connect(String),
    /// The connect/handshake/command exceeded [`QMP_TIMEOUT`].
    Timeout(&'static str),
    /// An I/O error reading or writing the stream.
    Io(String),
    /// The peer closed the stream before the expected reply arrived.
    Closed,
    /// A response line was not valid JSON.
    Parse(String),
    /// qemu returned `{"error":{"class","desc"},...}` for a command. node has
    /// no path for this (the command would silently never resolve); we surface
    /// it so stop/reset/info fail loudly instead of hanging.
    Command { class: String, desc: String },
}

impl std::fmt::Display for QmpError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            QmpError::Connect(e) => write!(f, "qmp connect failed: {e}"),
            QmpError::Timeout(what) => write!(f, "qmp {what} timed out"),
            QmpError::Io(e) => write!(f, "qmp io error: {e}"),
            QmpError::Closed => write!(f, "qmp socket closed before reply"),
            QmpError::Parse(e) => write!(f, "qmp parse error: {e}"),
            QmpError::Command { class, desc } => {
                write!(f, "qmp command error [{class}]: {desc}")
            }
        }
    }
}

impl std::error::Error for QmpError {}

/// A connected QMP monitor session, post-handshake and ready for commands.
/// One per control-verb invocation (connect-per-command, like node).
pub struct Qmp {
    reader: BufReader<OwnedReadHalf>,
    writer: OwnedWriteHalf,
    /// Monotonic command id (qmp.js `cmd_id`).
    next_id: u64,
}

impl Qmp {
    /// Connect to the monitor socket, consume the greeting, and complete the
    /// `qmp_capabilities` handshake. node `Qmp.connect` + `on_connect`
    /// (qmp.js:97/152). Bounded by [`QMP_TIMEOUT`] at every step.
    pub async fn connect(socket: &Path) -> Result<Self, QmpError> {
        let stream = tokio::time::timeout(QMP_TIMEOUT, UnixStream::connect(socket))
            .await
            .map_err(|_| QmpError::Timeout("connect"))?
            .map_err(|e| QmpError::Connect(e.to_string()))?;

        let (read, write) = stream.into_split();
        let mut qmp = Qmp {
            reader: BufReader::new(read),
            writer: write,
            next_id: 0,
        };

        // The server sends a greeting line `{"QMP":{...}}` first; skip
        // non-greeting lines defensively but expect exactly the banner.
        let greeting = qmp.read_line().await?;
        let parsed: Value = serde_json::from_str(greeting.trim())
            .map_err(|e| QmpError::Parse(e.to_string()))?;
        if parsed.get("QMP").is_none() {
            tracing::warn!(line = %greeting.trim(), "QMP first line was not a greeting");
        } else {
            tracing::debug!("QMP greeting received");
        }

        // Handshake: enable capabilities. Until this returns, no other
        // command may be sent (qmp.js:154).
        qmp.command("qmp_capabilities", None).await?;
        Ok(qmp)
    }

    /// Issue one command and return its `return` value. node `Qmp.command` +
    /// the id-matched return dispatch (qmp.js:175/108). Reads response lines,
    /// logging+skipping async events, until the matching id arrives or the
    /// command times out.
    pub async fn command(&mut self, command: &str, args: Option<Value>) -> Result<Value, QmpError> {
        let id = self.next_id;
        self.next_id += 1;

        let mut packet = json!({ "execute": command, "id": id });
        if let Some(args) = args {
            // node omits `arguments` when null (qmp.js:182).
            if let Value::Object(map) = &mut packet {
                map.insert("arguments".to_string(), args);
            }
        }

        let mut line = packet.to_string();
        line.push('\n');
        tokio::time::timeout(QMP_TIMEOUT, self.writer.write_all(line.as_bytes()))
            .await
            .map_err(|_| QmpError::Timeout("send"))?
            .map_err(|e| QmpError::Io(e.to_string()))?;

        // Read response lines until the matching id (qmp.js dispatch loop).
        loop {
            let resp = tokio::time::timeout(QMP_TIMEOUT, self.read_line())
                .await
                .map_err(|_| QmpError::Timeout("response"))??;
            let value: Value = serde_json::from_str(resp.trim())
                .map_err(|e| QmpError::Parse(e.to_string()))?;

            // Async event: log + keep reading (qmp.js:121).
            if value.get("event").is_some() {
                tracing::debug!(event = %resp.trim(), "QMP event");
                continue;
            }
            // A greeting could only appear pre-handshake; ignore if seen.
            if value.get("QMP").is_some() {
                continue;
            }
            // Error reply (node has no branch for this; we add one).
            if let Some(err) = value.get("error") {
                let class = err
                    .get("class")
                    .and_then(Value::as_str)
                    .unwrap_or("GenericError")
                    .to_string();
                let desc = err
                    .get("desc")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string();
                // Only consume an error addressed to our id; an id-less error
                // still aborts (it cannot be for a later command).
                return Err(QmpError::Command { class, desc });
            }
            if let Some(ret) = value.get("return") {
                let resp_id = value.get("id").and_then(Value::as_u64);
                if resp_id == Some(id) || resp_id.is_none() {
                    return Ok(ret.clone());
                }
                // A return for a different id: keep reading (defensive; in the
                // connect-per-command model only our ids are in flight).
                tracing::warn!(?resp_id, expected = id, "QMP return for unexpected id");
                continue;
            }
            // Anything else is unexpected; log and keep reading.
            tracing::warn!(line = %resp.trim(), "QMP unexpected message");
        }
    }

    /// Best-effort close (node `disconnect` => stream.end()).
    pub async fn disconnect(mut self) {
        let _ = self.writer.shutdown().await;
    }

    /// Read a single newline-terminated line from the stream. An EOF before a
    /// newline is a closed socket.
    async fn read_line(&mut self) -> Result<String, QmpError> {
        let mut buf = Vec::new();
        loop {
            let mut byte = [0u8; 1];
            let n = self
                .reader
                .read(&mut byte)
                .await
                .map_err(|e| QmpError::Io(e.to_string()))?;
            if n == 0 {
                return Err(QmpError::Closed);
            }
            if byte[0] == b'\n' {
                break;
            }
            buf.push(byte[0]);
        }
        String::from_utf8(buf).map_err(|e| QmpError::Parse(e.to_string()))
    }
}

/// The in-zone monitor socket path for a VM (`<zonepath>/root/tmp/vm.qmp`).
/// Every node QMP call uses this exact path (stopVM:1603, infoVM:1695, etc.).
#[must_use]
pub fn qmp_socket_path(zonepath: &Path) -> std::path::PathBuf {
    zonepath.join("root/tmp/vm.qmp")
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::AsyncWriteExt;
    use tokio::net::UnixListener;

    /// A fake QMP server that emits the greeting, answers qmp_capabilities,
    /// then runs `handler` over the post-handshake exchange.
    async fn fake_server<F>(path: std::path::PathBuf, handler: F)
    where
        F: FnOnce(Vec<String>) -> Vec<String> + Send + 'static,
    {
        let listener = UnixListener::bind(&path).expect("bind");
        tokio::spawn(async move {
            let (stream, _) = listener.accept().await.expect("accept");
            let (read, mut write) = stream.into_split();
            let mut reader = BufReader::new(read);
            // greeting
            write
                .write_all(b"{\"QMP\":{\"version\":{\"qemu\":{\"major\":2}}}}\n")
                .await
                .expect("greet");
            // read qmp_capabilities, answer it
            let cap = read_one_line(&mut reader).await;
            assert!(cap.contains("qmp_capabilities"), "got: {cap}");
            let cap_id = extract_id(&cap);
            write
                .write_all(format!("{{\"return\":{{}},\"id\":{cap_id}}}\n").as_bytes())
                .await
                .expect("cap return");
            // run the handler-driven exchange: it gets the request lines and
            // returns response lines to write back.
            let req = read_one_line(&mut reader).await;
            let responses = handler(vec![req]);
            for r in responses {
                write.write_all(r.as_bytes()).await.expect("resp");
            }
        });
    }

    async fn read_one_line(reader: &mut BufReader<OwnedReadHalf>) -> String {
        let mut buf = Vec::new();
        loop {
            let mut byte = [0u8; 1];
            let n = reader.read(&mut byte).await.expect("read");
            if n == 0 || byte[0] == b'\n' {
                break;
            }
            buf.push(byte[0]);
        }
        String::from_utf8(buf).expect("utf8")
    }

    fn extract_id(line: &str) -> u64 {
        let v: Value = serde_json::from_str(line).expect("json");
        v.get("id").and_then(Value::as_u64).expect("id")
    }

    #[tokio::test]
    async fn handshake_then_command_returns_value() {
        let dir = tempfile::tempdir().expect("tmp");
        let sock = dir.path().join("vm.qmp");
        fake_server(sock.clone(), |reqs| {
            let id = extract_id(&reqs[0]);
            assert!(reqs[0].contains("query-status"));
            vec![format!(
                "{{\"return\":{{\"running\":true,\"status\":\"running\"}},\"id\":{id}}}\n"
            )]
        })
        .await;

        let mut qmp = Qmp::connect(&sock).await.expect("connect");
        let res = qmp.command("query-status", None).await.expect("command");
        assert_eq!(res.get("status").and_then(Value::as_str), Some("running"));
        assert_eq!(res.get("running").and_then(Value::as_bool), Some(true));
    }

    #[tokio::test]
    async fn command_error_reply_is_surfaced() {
        let dir = tempfile::tempdir().expect("tmp");
        let sock = dir.path().join("vm.qmp");
        fake_server(sock.clone(), |reqs| {
            let id = extract_id(&reqs[0]);
            vec![format!(
                "{{\"error\":{{\"class\":\"CommandNotFound\",\"desc\":\"bogus\"}},\"id\":{id}}}\n"
            )]
        })
        .await;

        let mut qmp = Qmp::connect(&sock).await.expect("connect");
        let err = qmp.command("bogus", None).await.expect_err("should error");
        match err {
            QmpError::Command { class, desc } => {
                assert_eq!(class, "CommandNotFound");
                assert_eq!(desc, "bogus");
            }
            other => panic!("expected command error, got {other}"),
        }
    }

    #[tokio::test]
    async fn events_before_return_are_skipped() {
        let dir = tempfile::tempdir().expect("tmp");
        let sock = dir.path().join("vm.qmp");
        fake_server(sock.clone(), |reqs| {
            let id = extract_id(&reqs[0]);
            vec![
                "{\"event\":\"STOP\",\"timestamp\":{\"seconds\":1,\"microseconds\":2}}\n"
                    .to_string(),
                format!("{{\"return\":{{}},\"id\":{id}}}\n"),
            ]
        })
        .await;

        let mut qmp = Qmp::connect(&sock).await.expect("connect");
        let res = qmp
            .command("system_powerdown", None)
            .await
            .expect("command");
        assert_eq!(res, json!({}));
    }

    #[tokio::test]
    async fn connect_to_missing_socket_fails_fast() {
        let dir = tempfile::tempdir().expect("tmp");
        let sock = dir.path().join("nonexistent.qmp");
        // Qmp is not Debug, so match rather than expect_err.
        match Qmp::connect(&sock).await {
            Ok(_) => panic!("connect to a missing socket should fail"),
            Err(e) => assert!(matches!(e, QmpError::Connect(_))),
        }
    }

    #[test]
    fn socket_path_is_root_tmp_vm_qmp() {
        let p = qmp_socket_path(Path::new("/zones/abc"));
        assert_eq!(p, Path::new("/zones/abc/root/tmp/vm.qmp"));
    }
}
