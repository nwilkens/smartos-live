// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.
//
// Copyright 2026 Edgecast Cloud LLC.

//! `zoneevent` — native Rust drop-in for the SmartOS zoneevent CLI.
//!
//! Subscribes to zone state-change sysevents and prints one JSON line per
//! transition. The per-line schema matches node `zoneevent`:
//!
//! ```json
//! {"zonename":"<uuid>","oldstate":"ready","newstate":"running",
//!  "zoneid":"463","when":"1494617613097227838"}
//! ```
//!
//! Source of events: `/usr/sbin/sysevent -rj -c com.sun:zones:status status
//! change`, the same channel/class/subclass the historical zoneevent.c read
//! and that `sysevent-stream.js` consumes. Reading the channel directly avoids
//! the vminfod dependency while preserving node's output shape.

use std::io::{BufRead, BufReader, Write};
use std::process::{Child, Command, Stdio};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use clap::Parser;
use serde::Deserialize;
use serde_json::{Map, Value, json};

const SYSEVENT: &str = "/usr/sbin/sysevent";
const CHANNEL: &str = "com.sun:zones:status";
const CLASS: &str = "status";
const SUBCLASS: &str = "change";

// Match node: retry the watcher after a failure rather than exiting.
const RETRY: Duration = Duration::from_millis(1000);

#[derive(Parser)]
#[command(
    name = "zoneevent",
    about = "Emit one JSON line per zone state transition",
    version
)]
struct Cli {
    /// Identifier string for this invocation (accepted for compatibility).
    #[arg(short = 'i', long = "ident")]
    ident: Option<String>,
    /// Log level (accepted for compatibility; diagnostics go to stderr).
    #[arg(short = 'l', long = "level")]
    level: Option<String>,
}

/// One line of `sysevent -rj` output. Only the fields we act on are modelled;
/// the rest (vendor, publisher, class, subclass, pid, channel) are ignored.
#[derive(Deserialize)]
struct SysEvent {
    #[serde(rename = "type")]
    kind: String,
    date: Option<String>,
    data: Option<Map<String, Value>>,
}

fn main() -> std::process::ExitCode {
    let _cli = Cli::parse();

    // Mirror node's start()/error retry loop: on any watcher failure, log to
    // stderr and respawn after RETRY. A genuinely unspawnable sysevent binary
    // is fatal and reported once.
    loop {
        match watch() {
            Ok(()) => return std::process::ExitCode::SUCCESS,
            Err(e) => {
                eprintln!("zoneevent: {e:#}");
                if e.downcast_ref::<Fatal>().is_some() {
                    return std::process::ExitCode::FAILURE;
                }
                std::thread::sleep(RETRY);
            }
        }
    }
}

/// Non-recoverable error (e.g. the sysevent binary cannot be executed).
#[derive(Debug)]
struct Fatal(String);

impl std::fmt::Display for Fatal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for Fatal {}

fn watch() -> Result<()> {
    let mut child = spawn_sysevent()?;

    let stdout = child
        .stdout
        .take()
        .context("sysevent child has no stdout")?;
    let reader = BufReader::new(stdout);

    let stdout_handle = std::io::stdout();
    for line in reader.lines() {
        let line = line.context("reading sysevent stdout")?;
        if line.is_empty() {
            continue;
        }
        if let Some(obj) = transform(&line)? {
            let mut out = stdout_handle.lock();
            // One compact JSON object per line, matching node's console.log.
            serde_json::to_writer(&mut out, &obj).context("serializing event")?;
            out.write_all(b"\n").context("writing newline")?;
            out.flush().context("flushing stdout")?;
        }
    }

    // The child exited / closed its stdout: treat as a recoverable watcher
    // failure so the caller respawns (matches node's 'error' -> restart).
    let status = child.wait().context("waiting on sysevent child")?;
    bail!("sysevent exited: {status}");
}

fn spawn_sysevent() -> Result<Child> {
    Command::new(SYSEVENT)
        .args(["-rj", "-c", CHANNEL, CLASS, SUBCLASS])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .map_err(|e| Fatal(format!("failed to exec {SYSEVENT}: {e}")).into())
}

/// Turn one `sysevent -rj` line into the node zoneevent output object, or
/// `None` for lines that carry no zone transition (e.g. the `ready` event).
fn transform(line: &str) -> Result<Option<Value>> {
    let ev: SysEvent =
        serde_json::from_str(line).with_context(|| format!("parsing sysevent line: {line}"))?;

    if ev.kind != "event" {
        return Ok(None);
    }

    let data = match ev.data {
        Some(d) => d,
        None => return Ok(None),
    };

    let zonename = match data.get("zonename").and_then(Value::as_str) {
        Some(z) => z.to_string(),
        None => return Ok(None),
    };

    // newstate/oldstate are strings in the kernel zone-status nvlist. Default
    // to "" (as node does for create/delete edges) if absent.
    let newstate = string_field(&data, "newstate");
    let oldstate = string_field(&data, "oldstate");

    // node stringifies zoneid; the raw sysevent already carries it (int), so
    // use it directly and fall back to "-1" like node's getzoneidbyname catch.
    let zoneid = data
        .get("zoneid")
        .and_then(numeric_to_string)
        .unwrap_or_else(|| "-1".to_string());

    // node derives `when` from the event date (ms -> ns: ms * 1e6) rather than
    // the raw monotonic hrtime, so we reproduce that to match its output.
    let when = ev
        .date
        .as_deref()
        .and_then(when_from_date)
        .unwrap_or_default();

    Ok(Some(json!({
        "zonename": zonename,
        "oldstate": oldstate,
        "newstate": newstate,
        "zoneid": zoneid,
        "when": when,
    })))
}

fn string_field(data: &Map<String, Value>, key: &str) -> String {
    data.get(key)
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string()
}

/// Render a JSON numeric (or numeric string) value as a plain integer string.
fn numeric_to_string(v: &Value) -> Option<String> {
    match v {
        Value::Number(n) => Some(n.to_string()),
        Value::String(s) => Some(s.clone()),
        _ => None,
    }
}

/// Convert an RFC3339 millisecond timestamp into node's `<ms>000000` nanosecond
/// string. Returns None on unparseable input.
fn when_from_date(date: &str) -> Option<String> {
    let ms = epoch_millis(date)?;
    Some(format!("{ms}000000"))
}

/// Parse `YYYY-MM-DDTHH:MM:SS.mmmZ` (the sysevent date format) into epoch
/// milliseconds without pulling in a date crate.
fn epoch_millis(date: &str) -> Option<i64> {
    let date = date.strip_suffix('Z')?;
    let (d, t) = date.split_once('T')?;

    let mut dparts = d.split('-');
    let year: i64 = dparts.next()?.parse().ok()?;
    let month: i64 = dparts.next()?.parse().ok()?;
    let day: i64 = dparts.next()?.parse().ok()?;

    let (hms, frac) = match t.split_once('.') {
        Some((a, b)) => (a, b),
        None => (t, "0"),
    };
    let mut tparts = hms.split(':');
    let hour: i64 = tparts.next()?.parse().ok()?;
    let min: i64 = tparts.next()?.parse().ok()?;
    let sec: i64 = tparts.next()?.parse().ok()?;

    // Normalize fractional seconds to exactly 3 digits of milliseconds.
    let mut millis_str = String::from(frac);
    millis_str.truncate(3);
    while millis_str.len() < 3 {
        millis_str.push('0');
    }
    let millis: i64 = millis_str.parse().ok()?;

    let days = days_from_civil(year, month, day)?;
    let secs = days * 86_400 + hour * 3_600 + min * 60 + sec;
    Some(secs * 1_000 + millis)
}

/// Days since the Unix epoch for a civil (proleptic Gregorian) date.
/// Algorithm from Howard Hinnant's chrono date paper.
fn days_from_civil(y: i64, m: i64, d: i64) -> Option<i64> {
    if !(1..=12).contains(&m) || !(1..=31).contains(&d) {
        return None;
    }
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400; // [0, 399]
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d - 1; // [0, 365]
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy; // [0, 146096]
    Some(era * 146_097 + doe - 719_468)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ready_event_is_skipped() {
        let line = r#"{"date":"2026-06-23T23:32:06.226Z","type":"ready"}"#;
        assert!(transform(line).expect("parse").is_none());
    }

    #[test]
    fn transition_matches_node_schema() {
        let line = r#"{"date":"2017-05-12T19:33:33.097Z","type":"event",
            "vendor":"SUNW","publisher":"zfs","class":"status","subclass":"change",
            "pid":0,"data":{"zonename":"fb622681-3d62-413b-dc8a-c7515367464f",
            "newstate":"running","oldstate":"ready","when":1494617613097227838,
            "zoneid":463},"channel":"com.sun:zones:status"}"#;
        let obj = transform(line).expect("parse").expect("event");
        assert_eq!(obj["zonename"], "fb622681-3d62-413b-dc8a-c7515367464f");
        assert_eq!(obj["oldstate"], "ready");
        assert_eq!(obj["newstate"], "running");
        assert_eq!(obj["zoneid"], "463");
        assert_eq!(obj["when"], "1494617613097000000");
    }

    #[test]
    fn epoch_millis_known_value() {
        assert_eq!(epoch_millis("2017-05-12T19:33:33.097Z"), Some(1494617613097));
        assert_eq!(epoch_millis("1970-01-01T00:00:00.000Z"), Some(0));
    }

    #[test]
    fn missing_zoneid_defaults() {
        let line = r#"{"date":"2017-05-12T19:33:33.097Z","type":"event",
            "data":{"zonename":"z","newstate":"running","oldstate":"ready"}}"#;
        let obj = transform(line).expect("parse").expect("event");
        assert_eq!(obj["zoneid"], "-1");
    }
}
